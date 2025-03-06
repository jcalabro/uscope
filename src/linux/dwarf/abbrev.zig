//! Parses DWARF's .debug_abbrev tables

const std = @import("std");
const ArenaAllocator = std.heap.ArenaAllocator;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const AutoHashMapUnmanaged = std.AutoHashMapUnmanaged;
const assert = std.debug.assert;
const mem = std.mem;
const pow = std.math.pow;
const t = std.testing;

const consts = @import("consts.zig");
const dwarf = @import("../dwarf.zig");
const file = @import("../../file.zig");
const info = @import("info.zig");
const logging = @import("../../logging.zig");
const Offset = dwarf.Offset;
const Reader = @import("../../Reader.zig");
const trace = @import("../../trace.zig");

const log = logging.Logger.init(logging.Region.Symbols);

pub const Code = u64;

pub const Table = struct {
    offset: Offset,
    decls: AutoHashMapUnmanaged(Code, *Decl) = .{},

    pub fn getDecl(self: @This(), code: Code) error{InvalidDWARFInfo}!*Decl {
        const decl = self.decls.get(code);
        if (decl) |d| return d;

        log.errf("abbrev decl not found for code {d}", .{code});
        return error.InvalidDWARFInfo;
    }
};

pub const Decl = struct {
    tag: consts.AttributeTag,
    has_children: bool,
    attrs: ArrayListUnmanaged(Attr) = .{},
};

pub const Attr = struct {
    name: consts.AttributeName,
    form: consts.AttributeForm,
    implicit_const_val: i64 = 0,

    /// Determines which FormType to use for the given Attr, and advances the
    /// reader by the appropriate number of bytes
    pub fn chooseFormAndAdvanceBySize(
        self: @This(),
        val: *FormValue,
        cu: *info.CompileUnit,
    ) dwarf.ParseError!void {
        var form = self.form;
        if (self.form == .DW_FORM_indirect) {
            // Special case where the value of this form is actually a pointer to
            // a form of another type. There can be nested levels of indirection.
            const max = pow(usize, 2, 10);
            for (0..max) |indirect_ndx| {
                form = try dwarf.readEnumULEB128(cu.info_r, consts.AttributeForm);
                if (form != .DW_FORM_indirect) break;

                assert(indirect_ndx < max - 1);
            }
        }

        // most form types read from the current location in the .debug_info
        // section, and those that don't will just overwrite this value below
        val.offset = cu.info_r.offset() + cu.info_offset;

        switch (form) {
            // special-case must have already been handled above
            .DW_FORM_indirect => unreachable,

            .DW_FORM_data1 => {
                _ = try dwarf.read(cu.info_r, u8);
                val.form = FormU8.formType();
                val.class = .constant;
            },
            .DW_FORM_data2 => {
                _ = try dwarf.read(cu.info_r, u16);
                val.form = FormU16.formType();
                val.class = .constant;
            },
            .DW_FORM_data4 => {
                _ = try dwarf.read(cu.info_r, u32);
                val.form = FormU32.formType();
                val.class = .constant;
            },
            .DW_FORM_data8 => {
                _ = try dwarf.read(cu.info_r, u64);
                val.form = FormU64.formType();
                val.class = .constant;
            },
            .DW_FORM_data16 => {
                const data = try dwarf.read(cu.info_r, u128);
                val.form = FormStored.formType(@intCast(data));
                val.class = .constant;
            },
            .DW_FORM_sdata => {
                const data = try dwarf.readSLEB128(cu.info_r);
                val.form = FormStored.formType(data);
                val.class = .constant;
            },
            .DW_FORM_udata => {
                const data = try dwarf.readULEB128(cu.info_r);
                val.form = FormStored.formType(data);
                val.class = .constant;
            },

            .DW_FORM_addr => {
                _ = try dwarf.read(cu.info_r, usize);
                val.form = FormOffset.formType();
                val.class = .address;
            },
            .DW_FORM_addrx1 => {
                try addrx(val, cu, try dwarf.read(cu.info_r, u8));
            },
            .DW_FORM_addrx2 => {
                try addrx(val, cu, try dwarf.read(cu.info_r, u16));
            },
            .DW_FORM_addrx3 => {
                try addrx(val, cu, try dwarf.read(cu.info_r, u32));
            },
            .DW_FORM_addrx4 => {
                try addrx(val, cu, try dwarf.read(cu.info_r, u64));
            },
            .DW_FORM_addrx => {
                try addrx(val, cu, try dwarf.readULEB128(cu.info_r));
            },

            .DW_FORM_ref_addr => {
                _ = try cu.readInfoOffset();
                val.form = FormOffset.formType();
                val.class = .global_reference;
            },
            .DW_FORM_ref1 => {
                _ = try dwarf.read(cu.info_r, u8);
                val.form = FormU8.formType();
                val.class = .reference;
            },
            .DW_FORM_ref2 => {
                _ = try dwarf.read(cu.info_r, u16);
                val.form = FormU16.formType();
                val.class = .reference;
            },
            .DW_FORM_ref4 => {
                _ = try dwarf.read(cu.info_r, u32);
                val.form = FormU32.formType();
                val.class = .reference;
            },
            .DW_FORM_ref8 => {
                _ = try dwarf.read(cu.info_r, u64);
                val.form = FormU64.formType();
                val.class = .reference;
            },
            .DW_FORM_ref_udata => {
                const data = try dwarf.readULEB128(cu.info_r);
                val.form = FormStored.formType(data);
                val.class = .reference;
            },

            .DW_FORM_string => {
                const len = try findStringLen(cu.info_r);
                val.form = FormString.formType(.info, len);
                val.class = .string;
            },
            .DW_FORM_strp => {
                val.offset = try cu.readInfoOffset();
                var r: Reader = undefined;
                r.init(cu.opts.sections.str.contents);
                r.seek(val.offset);
                const len = try findStringLen(&r);
                val.form = FormString.formType(.str, len);
                val.class = .string;
            },
            .DW_FORM_line_strp => {
                if (cu.header.version.isLessThan(.five)) {
                    log.errf(
                        "attempted to read a DW_FORM_line_strp from a DWARF v{d} file (DW_FORM_line_strp was introduced in v5)",
                        .{cu.header.version.int()},
                    );
                    return error.InvalidDWARFInfo;
                }

                val.offset = try cu.readInfoOffset();
                var r: Reader = undefined;
                r.init(cu.opts.sections.line_str.contents);
                r.seek(val.offset);
                const len = try findStringLen(&r);
                val.form = FormString.formType(.line_str, len);
                val.class = .string;
            },
            // .DW_FORM_strp_sup => {}, // supplementary object files not yet implemented
            .DW_FORM_strx1 => {
                try strx(val, cu, try dwarf.read(cu.info_r, u8));
            },
            .DW_FORM_strx2 => {
                try strx(val, cu, try dwarf.read(cu.info_r, u16));
            },
            .DW_FORM_strx3 => {
                try strx(val, cu, try dwarf.read(cu.info_r, u32));
            },
            .DW_FORM_strx4 => {
                try strx(val, cu, try dwarf.read(cu.info_r, u64));
            },
            .DW_FORM_strx => {
                try strx(val, cu, try dwarf.readULEB128(cu.info_r));
            },

            .DW_FORM_flag => {
                val.form = FormStored.formType(try dwarf.read(cu.info_r, u8));
                val.class = .flag;
            },
            .DW_FORM_flag_present => {
                // flag is implicitly true
                val.form = FormStored.formType(1);
                val.class = .flag;
            },
            .DW_FORM_implicit_const => {
                val.form = FormStored.formType(self.implicit_const_val);
                val.class = .constant;
            },

            .DW_FORM_block1 => {
                const len = try dwarf.read(cu.info_r, u8);
                val.offset = cu.info_offset + cu.info_r.offset();
                cu.info_r.seek(cu.info_r.offset() + len);
                val.form = FormString.formType(.info, len);
                val.class = .block;
            },
            .DW_FORM_block2 => {
                const len = try dwarf.read(cu.info_r, u16);
                val.offset = cu.info_offset + cu.info_r.offset();
                cu.info_r.seek(cu.info_r.offset() + len);
                val.form = FormString.formType(.info, len);
                val.class = .block;
            },
            .DW_FORM_block4 => {
                const len = try dwarf.read(cu.info_r, u32);
                val.offset = cu.info_offset + cu.info_r.offset();
                cu.info_r.seek(cu.info_r.offset() + len);
                val.form = FormString.formType(.info, len);
                val.class = .block;
            },
            .DW_FORM_block, .DW_FORM_exprloc => {
                const len = try dwarf.readULEB128(cu.info_r);
                val.offset = cu.info_offset + cu.info_r.offset();
                cu.info_r.seek(cu.info_r.offset() + len);
                val.form = FormString.formType(.info, len);
                val.class = .block;

                if (form == .DW_FORM_exprloc) val.class = .exprloc;
            },

            // @NEEDSTEST: test all these classes
            .DW_FORM_sec_offset => {
                // offset in to a one of many sections depending on the type of abbrev Attribute
                val.offset = try cu.readInfoOffset();
                val.form = FormStored.formType(val.offset);

                val.class = switch (self.name) {
                    // addrptr class
                    .DW_AT_addr_base,
                    .DW_AT_GNU_addr_base,
                    => .addrptr,

                    // lineptr class
                    .DW_AT_stmt_list => .lineptr,

                    // loclist class (was called loclists ptr up through DWARF v4)
                    .DW_AT_location,
                    .DW_AT_string_length,
                    .DW_AT_return_addr,
                    .DW_AT_data_member_location,
                    .DW_AT_frame_base,
                    .DW_AT_segment,
                    .DW_AT_static_link,
                    .DW_AT_use_location,
                    .DW_AT_vtable_elem_location,
                    .DW_AT_GNU_locviews,
                    => c: {
                        if (cu.header.version.isLessThan(.five)) break :c .loclistptr;
                        break :c .loclist;
                    },

                    // loclistsptr class
                    .DW_AT_loclists_base => .loclistptr,

                    // macptr class (class name stayed the same, but the section
                    // that stores macro data changed from "macinfo" to "macro" in DWARF v5)
                    .DW_AT_macro_info,
                    .DW_AT_macros,
                    .DW_AT_mac_info,
                    .DW_AT_GNU_macros,
                    => .macptr,

                    // rnglist class (was called rangelistptr up through DWARF v4, and the
                    // sectio change from the "ranges" section to the "rnglists" section as
                    // of DWARF v5)
                    .DW_AT_start_scope,
                    .DW_AT_ranges,
                    .DW_AT_rnglists_base,
                    .DW_AT_GNU_ranges_base,
                    => .rnglist,

                    // stroffsetsptr class (introduced in DWARF v4)
                    .DW_AT_str_offsets_base => .stroffsetptr,

                    else => {
                        log.errf("invalid DW_FORM_sec_offset attribute: {any}", .{self.name});
                        return error.InvalidDWARFInfo;
                    },
                };
            },

            .DW_FORM_rnglistx => {
                // @SRC (jrc): https://go.dev/src/debug/dwarf/entry.go?s=26069:26121
                // offset is from DW_AT_rnglists_base, NOT from the start of the rnglists section
                var offset_loc = try dwarf.readULEB128(cu.info_r);
                offset_loc *= cu.header.addr_size.bytes();
                offset_loc += cu.rnglists_base;

                if (offset_loc >= cu.opts.sections.rnglists.contents.len) {
                    log.err("DW_FORM_rnglistx offset out of bounds");
                    return error.InvalidDWARFInfo;
                }

                var r: Reader = undefined;
                r.init(cu.opts.sections.rnglists.contents);
                r.seek(offset_loc);
                const offset = switch (cu.header.addr_size) {
                    .four => try dwarf.read(&r, u32),
                    .eight => try dwarf.read(&r, u64),
                };

                val.offset = cu.rnglists_base + offset;
                val.class = .rnglist;
            },

            else => {
                // @TODO (jrc): implement all other form types
                log.errf("unimplemented form type: {s}", .{@tagName(form)});
                return error.InvalidDWARFInfo;
            },
        }

        assert(@intFromEnum(val.class) <= @typeInfo(FormClass).@"enum".fields.len);
    }

    /// Looks up a string in the .debug_str table via an offset in
    /// the .debug_str_offsets table
    /// @NEEDSTEST
    fn strx(
        val: *FormValue,
        cu: *const info.CompileUnit,
        str_offset: Offset,
    ) dwarf.ParseError!void {
        //
        // Look up the offset in to the .debug_str section using the .debug_str_offsets section
        //

        if (str_offset >= cu.str_offsets_table.len) {
            log.errf("string offset {d} out of range (max {d})", .{
                str_offset,
                cu.str_offsets_table.len,
            });
            return error.InvalidDWARFInfo;
        }

        const offset = cu.str_offsets_table[str_offset];

        //
        // Read the length of the string in the .debug_str section
        //

        if (str_offset >= cu.opts.sections.str_offsets.contents.len) {
            log.errf(".debug_str offset 0{x} out of range (len 0x{x})", .{
                offset,
                cu.opts.sections.str.contents.len,
            });
            return error.InvalidDWARFInfo;
        }

        var str_r: Reader = undefined;
        str_r.init(cu.opts.sections.str.contents[offset..]);
        const len = try findStringLen(&str_r);

        val.offset = offset;
        val.form = FormString.formType(.str, len);
        val.class = .string;
    }

    /// Looks up an address in the .debug_addr table for this compile unit in the
    /// same way that strx looks up in the .debug_str_offsets table
    /// @NEEDSTEST
    fn addrx(
        val: *FormValue,
        cu: *const info.CompileUnit,
        addr_offset: Offset,
    ) dwarf.ParseError!void {
        if (addr_offset >= cu.addr_table.len) {
            log.errf(".debug_addr offset 0{x} out of range (len 0x{x})", .{
                addr_offset,
                cu.addr_table,
            });
        }

        const addr = cu.addr_table[addr_offset];
        val.form = FormStored.formType(addr);
        val.class = .address;
    }
};

/// Reads all data from the .debug_abbrev section in to tables for each compilation unit
pub fn parse(opts: *const dwarf.ParseOpts) dwarf.ParseError![]Table {
    const z = trace.zoneN(@src(), "parse abbrev");
    defer z.end();

    const max = pow(usize, 2, 24);
    assert(opts.sections.abbrev.contents.len < max);

    var tables: ArrayListUnmanaged(Table) = .{};

    var r: Reader = undefined;
    r.init(opts.sections.abbrev.contents);

    while (r.offset() < opts.sections.abbrev.contents.len) {
        const table = try tables.addOne(opts.scratch);
        table.* = .{ .offset = r.offset() };

        for (0..max) |decl_ndx| {
            const code = try dwarf.readULEB128(&r);
            if (code == 0) break;

            const tag = try dwarf.readEnumULEB128(&r, consts.AttributeTag);
            const has_children = (try dwarf.read(&r, u8)) > 0;

            const decl = try opts.scratch.create(Decl);
            try table.decls.put(opts.scratch, code, decl);
            decl.* = .{
                .tag = tag,
                .has_children = has_children,
            };

            for (0..max) |attr_ndx| {
                const name = try dwarf.readULEB128(&r);
                const form = try dwarf.readULEB128(&r);
                if (name == 0 and form == 0) break;

                const attr = try decl.attrs.addOne(opts.scratch);
                attr.* = .{
                    .name = try dwarf.safeEnumFromInt(consts.AttributeName, name),
                    .form = try dwarf.safeEnumFromInt(consts.AttributeForm, form),
                };

                if (attr.form == .DW_FORM_implicit_const) {
                    attr.implicit_const_val = try dwarf.readSLEB128(&r);
                }

                assert(attr_ndx < max - 1);
            }

            assert(decl_ndx < max - 1);
        }
    }

    return tables.toOwnedSlice(opts.scratch);
}

const cloop = @embedFile("../test_files/linux_x86-64_cloop_out_abbrev");

test "parse errors" {
    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const alloc = arena.allocator();

    var sections = mem.zeroes(dwarf.Sections);
    var abbrev = try alloc.alloc(u8, cloop.len);
    @memcpy(abbrev, cloop);
    sections.abbrev.contents = abbrev;
    try t.expectEqual(@as(usize, 0x13a), sections.abbrev.contents.len);

    const fc = try file.Cache.init(t.allocator);
    defer fc.deinit();

    const opts = dwarf.ParseOpts{
        .scratch = alloc,
        .sections = &sections,
        .file_cache = fc,
    };

    {
        // manually set an invalid attribute tag
        const val = abbrev[1];
        defer abbrev[1] = val;
        abbrev[1] = 0xad;

        try t.expectError(error.InvalidDWARFInfo, parse(&opts));
    }

    {
        // manually set an invalid attribute name
        const val = abbrev[4];
        defer abbrev[4] = val;
        abbrev[4] = 0xad;

        try t.expectError(error.InvalidDWARFInfo, parse(&opts));
    }

    {
        // manually set an invalid attribute value
        const val = abbrev[5];
        defer abbrev[5] = val;
        abbrev[5] = 0xad;

        try t.expectError(error.InvalidDWARFInfo, parse(&opts));
    }
}

test "parse cloop abbrev table" {
    var sections = mem.zeroes(dwarf.Sections);
    sections.abbrev.contents = cloop;
    try t.expectEqual(@as(usize, 0x13a), sections.abbrev.contents.len);

    const fc = try file.Cache.init(t.allocator);
    defer fc.deinit();

    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();

    const tables = try parse(&.{
        .scratch = arena.allocator(),
        .sections = &sections,
        .file_cache = fc,
    });

    try t.expectEqual(@as(usize, 1), tables.len);

    const table = tables[0];
    try t.expectEqual(@as(usize, 22), table.decls.count());

    //
    // Spot check a few decls and their attributes
    //

    {
        // 1st entry (decl codes start counting at 1)
        const decl = try table.getDecl(1);
        try t.expectEqual(consts.AttributeTag.DW_TAG_member, decl.tag);
        try t.expectEqual(false, decl.has_children);
        try t.expectEqual(@as(usize, 6), decl.attrs.items.len);

        const expected = [_]Attr{
            .{ .name = .DW_AT_name, .form = .DW_FORM_strp },
            .{
                .name = .DW_AT_decl_file,
                .form = .DW_FORM_implicit_const,
                .implicit_const_val = 0x5,
            },
            .{ .name = .DW_AT_decl_line, .form = .DW_FORM_data1 },
            .{ .name = .DW_AT_decl_column, .form = .DW_FORM_data1 },
            .{ .name = .DW_AT_type, .form = .DW_FORM_ref4 },
            .{ .name = .DW_AT_data_member_location, .form = .DW_FORM_data1 },
        };
        try t.expectEqual(expected.len, decl.attrs.items.len);

        for (expected, 0..) |exp, ndx| {
            try t.expectEqual(exp, decl.attrs.items[ndx]);
        }
    }

    {
        // 20th entry has no entries
        const decl = try table.getDecl(20);
        try t.expectEqual(consts.AttributeTag.DW_TAG_unspecified_parameters, decl.tag);
        try t.expectEqual(false, decl.has_children);
        try t.expectEqual(@as(usize, 0), decl.attrs.items.len);
    }

    {
        // final entry
        const decl = try table.getDecl(22);
        try t.expectEqual(consts.AttributeTag.DW_TAG_subprogram, decl.tag);
        try t.expectEqual(true, decl.has_children);
        try t.expectEqual(@as(usize, 11), decl.attrs.items.len);

        const expected = [_]Attr{
            .{ .name = .DW_AT_external, .form = .DW_FORM_flag_present },
            .{ .name = .DW_AT_name, .form = .DW_FORM_strp },
            .{ .name = .DW_AT_decl_file, .form = .DW_FORM_data1 },
            .{ .name = .DW_AT_decl_line, .form = .DW_FORM_data1 },
            .{ .name = .DW_AT_decl_column, .form = .DW_FORM_data1 },
            .{ .name = .DW_AT_type, .form = .DW_FORM_ref4 },
            .{ .name = .DW_AT_low_pc, .form = .DW_FORM_addr },
            .{ .name = .DW_AT_high_pc, .form = .DW_FORM_data8 },
            .{ .name = .DW_AT_frame_base, .form = .DW_FORM_exprloc },
            .{ .name = .DW_AT_call_all_tail_calls, .form = .DW_FORM_flag_present },
            .{ .name = .DW_AT_sibling, .form = .DW_FORM_ref4 },
        };
        try t.expectEqual(expected.len, decl.attrs.items.len);

        for (expected, 0..) |exp, ndx| {
            try t.expectEqual(exp, decl.attrs.items[ndx]);
        }
    }

    //
    // Test some invalid attribute code lookups
    //

    try t.expectError(error.InvalidDWARFInfo, table.getDecl(0));
    try t.expectError(error.InvalidDWARFInfo, table.getDecl(23));
}

test "parse zigloop abbrev table" {
    const zigloop = @embedFile("../test_files/linux_x86-64_zigloop_out_abbrev");
    assert(zigloop.len > 0);

    var sections = mem.zeroes(dwarf.Sections);
    sections.abbrev.contents = zigloop;
    try t.expectEqual(@as(usize, 0x7fb), sections.abbrev.contents.len);

    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();

    const fc = try file.Cache.init(t.allocator);
    defer fc.deinit();

    const tables = try parse(&.{
        .scratch = arena.allocator(),
        .sections = &sections,
        .file_cache = fc,
    });

    try t.expectEqual(@as(usize, 2), tables.len);

    {
        const table = tables[0];
        try t.expectEqual(@as(Offset, 0x0), table.offset);
        try t.expectEqual(@as(usize, 61), table.decls.count());

        {
            // check the first entry
            const decl = try table.getDecl(1);
            try t.expectEqual(consts.AttributeTag.DW_TAG_compile_unit, decl.tag);
            try t.expectEqual(true, decl.has_children);
            try t.expectEqual(@as(usize, 8), decl.attrs.items.len);

            const expected = [_]Attr{
                .{ .name = .DW_AT_producer, .form = .DW_FORM_strp },
                .{ .name = .DW_AT_language, .form = .DW_FORM_data2 },
                .{ .name = .DW_AT_name, .form = .DW_FORM_strp },
                .{ .name = .DW_AT_stmt_list, .form = .DW_FORM_sec_offset },
                .{ .name = .DW_AT_comp_dir, .form = .DW_FORM_strp },
                .{ .name = .DW_AT_GNU_pubnames, .form = .DW_FORM_flag_present },
                .{ .name = .DW_AT_low_pc, .form = .DW_FORM_addr },
                .{ .name = .DW_AT_ranges, .form = .DW_FORM_sec_offset },
            };
            try t.expectEqual(expected.len, decl.attrs.items.len);

            for (expected, 0..) |exp, ndx| {
                try t.expectEqual(exp, decl.attrs.items[ndx]);
            }
        }

        {
            // check an inlined subroutine
            const decl = try table.getDecl(59);
            try t.expectEqual(consts.AttributeTag.DW_TAG_inlined_subroutine, decl.tag);
            try t.expectEqual(false, decl.has_children);
            try t.expectEqual(@as(usize, 5), decl.attrs.items.len);

            const expected = [_]Attr{
                .{ .name = .DW_AT_abstract_origin, .form = .DW_FORM_ref4 },
                .{ .name = .DW_AT_ranges, .form = .DW_FORM_sec_offset },
                .{ .name = .DW_AT_call_file, .form = .DW_FORM_data1 },
                .{ .name = .DW_AT_call_line, .form = .DW_FORM_data1 },
                .{ .name = .DW_AT_call_column, .form = .DW_FORM_data1 },
            };
            try t.expectEqual(expected.len, decl.attrs.items.len);

            for (expected, 0..) |exp, ndx| {
                try t.expectEqual(exp, decl.attrs.items[ndx]);
            }
        }
    }

    {
        const table = tables[1];
        try t.expectEqual(@as(Offset, 0x386), table.offset);
        try t.expectEqual(@as(usize, 79), table.decls.count());

        {
            // check a lexical block
            const decl = try table.getDecl(20);
            try t.expectEqual(consts.AttributeTag.DW_TAG_lexical_block, decl.tag);
            try t.expectEqual(true, decl.has_children);
            try t.expectEqual(@as(usize, 0), decl.attrs.items.len);
        }

        {
            // check a formal parameter
            const decl = try table.getDecl(47);
            try t.expectEqual(consts.AttributeTag.DW_TAG_formal_parameter, decl.tag);
            try t.expectEqual(false, decl.has_children);
            try t.expectEqual(@as(usize, 5), decl.attrs.items.len);

            const expected = [_]Attr{
                .{ .name = .DW_AT_location, .form = .DW_FORM_exprloc },
                .{ .name = .DW_AT_name, .form = .DW_FORM_strp },
                .{ .name = .DW_AT_decl_file, .form = .DW_FORM_data1 },
                .{ .name = .DW_AT_decl_line, .form = .DW_FORM_data2 },
                .{ .name = .DW_AT_type, .form = .DW_FORM_ref4 },
            };
            try t.expectEqual(expected.len, decl.attrs.items.len);

            for (expected, 0..) |exp, ndx| {
                try t.expectEqual(exp, decl.attrs.items[ndx]);
            }
        }
    }
}

/// FormValue is an instance of a FormType whose data can be retrieved according to its data type and the section
/// in which the bytes are stored. Parsing any type of data never allocates because we are simply reading memory
/// that has already been mmaped.
pub const FormValue = struct {
    offset: Offset,
    name: consts.AttributeName,
    form: FormType,
    class: FormClass,

    pub fn parseNumeric(self: @This(), comptime T: type, cu: *const info.CompileUnit) error{InvalidDWARFInfo}!T {
        return switch (self.form) {
            .string => {
                log.errf("cannot parse form type {s} to numeric", .{@tagName(self.name)});
                return error.InvalidDWARFInfo;
            },
            inline else => |f| {
                return switch (T) {
                    bool => (try f.parse(cu, self.offset)) > 0,
                    else => @intCast(try f.parse(cu, self.offset)),
                };
            },
        };
    }
};

pub const FormClass = enum(u8) {
    address,
    addrptr,
    block,
    constant,
    exprloc,
    flag,
    lineptr,
    loclist,
    loclistptr,
    macptr,
    rnglist,
    rnglistptr,
    reference,
    string,
    stroffsetptr,

    /// @NOTE (jrc): This is not a real class in the DWARF spec. I added it to differentiate
    /// between `DW_FORM_ref_addr` and `DW_FORM_ref*`, which is needed when determining if an
    /// offset is global (from the start of the `.debug_info` section), or local (from the start
    /// of the compile unit DIE).
    global_reference,

    pub fn contents(self: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}![]const u8 {
        const buf = switch (self) {
            .address, .reference => cu.opts.sections.info.contents,
            .addrptr => cu.opts.sections.addr.contents,
            .loclist => cu.opts.sections.loclists.contents,
            .loclistptr => cu.opts.sections.loclists.contents,
            .stroffsetptr => cu.opts.sections.str_offsets.contents,

            // section name changed in DWARF v5
            .macptr => blk: {
                if (cu.header.version.isAtLeast(.five)) {
                    break :blk cu.opts.sections.macro.contents;
                } else {
                    break :blk cu.opts.sections.macinfo.contents;
                }
            },

            // section and class name changed in DWARF v5
            .rnglist => blk: {
                if (cu.header.version.isAtLeast(.five)) {
                    break :blk cu.opts.sections.rnglists.contents;
                } else {
                    break :blk cu.opts.sections.ranges.contents;
                }
            },

            else => {
                log.errf("invalid section type in FormClass.parse: {s}", .{@tagName(self)});
                return error.InvalidDWARFInfo;
            },
        };

        // unfortunately, we don't know what the upper bound of this return value should be at this time
        return buf[offset..];
    }
};

/// Data in the .debug_abbrev section is a long array of contiguous bytes that should be
/// interpreted in many ways depending on the type of Form of the Attribute. This enum contains
/// all the ways we could parse a given set of bytes. There are many more DW_FORM types, but
/// only so many ways to interpret those bytes.
pub const FormType = union(enum) {
    u8: FormU8,
    u16: FormU16,
    u32: FormU32,
    u64: FormU64,
    i8: FormI8,
    i16: FormI16,
    i32: FormI32,
    i64: FormI64,
    offset: FormOffset,
    string: FormString,
    stored: FormStored,
};

pub const FormU8 = struct {
    pub fn formType() FormType {
        return FormType{ .u8 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!u8 {
        try numericFormBoundsCheck(u8, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(u8, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormU16 = struct {
    pub fn formType() FormType {
        return FormType{ .u16 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!u16 {
        try numericFormBoundsCheck(u16, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(u16, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormU32 = struct {
    pub fn formType() FormType {
        return FormType{ .u32 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!u32 {
        try numericFormBoundsCheck(u32, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(u32, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormU64 = struct {
    pub fn formType() FormType {
        return FormType{ .u64 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!u64 {
        try numericFormBoundsCheck(u64, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(u64, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormI8 = struct {
    pub fn formType() FormType {
        return FormType{ .i8 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!i8 {
        try numericFormBoundsCheck(i8, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(i8, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormI16 = struct {
    pub fn formType() FormType {
        return FormType{ .i16 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!i16 {
        try numericFormBoundsCheck(i16, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(i16, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormI32 = struct {
    pub fn formType() FormType {
        return FormType{ .i32 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!i32 {
        try numericFormBoundsCheck(i32, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(i32, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormI64 = struct {
    pub fn formType() FormType {
        return FormType{ .i64 = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!i64 {
        try numericFormBoundsCheck(i64, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(i64, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormOffset = struct {
    pub fn formType() FormType {
        return FormType{ .offset = @This(){} };
    }

    pub fn parse(_: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}!Offset {
        if (cu.header.is_32_bit) {
            try numericFormBoundsCheck(u32, cu.opts.sections.info.contents, offset);
            return mem.bytesToValue(u32, cu.opts.sections.info.contents[offset..]);
        }

        try numericFormBoundsCheck(u64, cu.opts.sections.info.contents, offset);
        return mem.bytesToValue(u64, cu.opts.sections.info.contents[offset..]);
    }
};

pub const FormString = struct {
    len: usize,
    section: FormStringSection,

    pub fn formType(section: FormStringSection, len: usize) FormType {
        return FormType{ .string = @This(){
            .len = len,
            .section = section,
        } };
    }

    /// Returns a slice in to the existing debug_info_contents array, so no allocations are performed
    pub fn parse(self: @This(), cu: *const info.CompileUnit, offset: Offset) error{InvalidDWARFInfo}![]const u8 {
        const section_data = self.section.contents(cu.opts.sections);

        const end = offset + self.len;
        if (end >= section_data.len) {
            log.err("string data out of bounds");
            return error.InvalidDWARFInfo;
        }

        return section_data[offset..end];
    }
};

/// FormStored is a special case where we store the variable value on the struct itself rather
/// than searching through the .debug_info or other sections to find it. This is used for special
/// cases such as DW_FORM_implicit_const and DW_FORM_flag_present that have no data represented
/// in the .debug_info section. This is also useful for storing signed/unsigned LEB128 numbers
/// so we don't need to convert them again later on.
pub const FormStored = struct {
    val: i128,

    pub fn formType(val: i128) FormType {
        return FormType{ .stored = @This(){ .val = val } };
    }

    pub fn parse(self: @This(), _: *const info.CompileUnit, _: Offset) error{InvalidDWARFInfo}!i128 {
        return self.val;
    }
};

/// Strings in DWARF may be in one of several sections. This.contents enum details which section in which
/// a string at a given offset may be found. The sections here may be data types other than the
/// "strings", but they are all arrays of bytes.
pub const FormStringSection = enum(u8) {
    abbrev,
    line,
    info,
    addr,
    aranges,
    frame,
    line_str,
    loc,
    loclists,
    names,
    macinfo,
    macro,
    pubnames,
    pubtypes,
    ranges,
    rnglists,
    str,
    str_offsets,
    types,

    pub fn contents(self: @This(), sections: *const dwarf.Sections) []const u8 {
        return switch (self) {
            .abbrev => return sections.abbrev.contents,
            .line => return sections.line.contents,
            .info => return sections.info.contents,
            .addr => return sections.addr.contents,
            .aranges => return sections.aranges.contents,
            .frame => return sections.frame.contents,
            .line_str => return sections.line_str.contents,
            .loc => return sections.loc.contents,
            .loclists => return sections.loclists.contents,
            .names => return sections.names.contents,
            .macinfo => return sections.macinfo.contents,
            .macro => return sections.macro.contents,
            .pubnames => return sections.pubnames.contents,
            .pubtypes => return sections.pubtypes.contents,
            .ranges => return sections.ranges.contents,
            .rnglists => return sections.rnglists.contents,
            .str => return sections.str.contents,
            .str_offsets => return sections.str_offsets.contents,
            .types => return sections.types.contents,
        };
    }
};

fn numericFormBoundsCheck(comptime T: type, buf: []const u8, offset: Offset) error{InvalidDWARFInfo}!void {
    const end = offset + @sizeOf(T);
    if (end >= buf.len) {
        log.errf("numeric form value of type {s} out of range at offset 0x{x}", .{
            @typeName(T),
            offset,
        });
        return error.InvalidDWARFInfo;
    }
}

/// @PERFORMANCE (jrc)
fn findStringLen(r: *Reader) error{InvalidDWARFInfo}!usize {
    const start = r.offset();

    const max = pow(usize, 2, 16);
    for (0..max) |ndx| {
        const ch = try dwarf.read(r, u8);
        if (ch == 0) break;

        assert(ndx < max - 1);
    }

    return r.offset() - start - 1;
}

test "findStringLen" {
    var r: Reader = undefined;

    r.init(&[_]u8{});
    try t.expectError(error.InvalidDWARFInfo, findStringLen(&r));

    r.init(&[_]u8{0});
    try t.expectEqual(@as(usize, 0), findStringLen(&r));

    // doesn't end in a null terminator
    r.init("hello");
    try t.expectError(error.InvalidDWARFInfo, findStringLen(&r));

    r.init("hello\x00");
    try t.expectEqual(@as(usize, 5), findStringLen(&r));
}
