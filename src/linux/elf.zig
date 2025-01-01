//! Contains code for safely parsing ELF binaries. We wrote our own version rather than relying on
//! the standard library because it is important that we never crash under unexpected input, which
//! is not necessarily a goal of other librares (i.e. the zig stdlib just loads the bits and casts).

const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const compress = std.compress;
const fs = std.fs;
const heap = std.heap;
const io = std.io;
const math = std.math;
const mem = std.mem;
const os = std.os;
const stdelf = std.elf;
const t = std.testing;

const consts = @import("elf_consts.zig");
const dwarf = @import("dwarf.zig");
const file = @import("../file.zig");
const flags = @import("../flags.zig");
const logging = @import("../logging.zig");
const Reader = @import("../Reader.zig");
const safe = @import("../safe.zig");
const strings = @import("../strings.zig");
const String = strings.String;
const trace = @import("../trace.zig");
const types = @import("../types.zig");

const log = logging.Logger.init(logging.Region.Symbols);

//
// @NOTE (jrc): This is heavily inspired by the Zig and Go standard libraries
//
// 1. https://refspecs.linuxfoundation.org/elf/gabi4+/contents.html
// 2. https://ziglang.org/documentation/master/std/src/elf.zig.html
// 3. https://pkg.go.dev/debug/elf#NewFile
//

/// Ident is the header at the start of the ELF file that describes several
/// header fields about the executable
const Ident = struct {
    class: consts.Class = .none, // elfconsts.EI_CLASS
    data: consts.Data = .unknown, // elfconsts.EI_DATA (byte order)
    version: consts.Version = .unknown, // elfconsts.EI_VERSION
    os_abi: consts.OSABI = .sysv, // elfconsts.EI_OSABI (sysv shares the zero value with "none")
    abi_version: u8 = 0, // elfconsts.EI_ABIVERSION
};

const Section32 = struct {
    name: ArrayList(u8) = undefined,
    type: consts.SectionType = .null,
    flags: u32 = 0,
    addr: usize = 0,
    offset: usize = 0,
    size: u32 = 0,
    link: u32 = 0,
    extra_info: u32 = 0,
    addr_align: u32 = 0,
    ent_size: u32 = 0,
};

const Section64 = struct {
    name: String = undefined,
    type: consts.SectionType = .null,
    flags: u64 = 0,
    addr: usize = 0,
    offset: usize = 0,
    size: u64 = 0,
    link: u32 = 0,
    extra_info: u32 = 0,
    addr_align: u64 = 0,
    ent_size: u64 = 0,
};

/// Section is either a 32 or 64 bit header that describes the location
/// of the section within the executable and some other metadata
/// @TODO (jrc): these should be selected at runtime, not compile-time
const Section = switch (@sizeOf(usize)) {
    4 => Section32,
    8 => Section64,
    else => @compileError("expected pointer size of 4 or 8 bytes"),
};

const CompressedSectionHeader = struct {
    /// Compression format (1 is zlib/deflate, all other values are os/platform specific or user-defined)
    ch_type: u32,
    /// Unused, and not present at all in 32-bit ELF
    ch_reserved: u32,
    /// Uncompressed data size
    ch_size: u64,
    /// Uncompressed data alignment
    ch_addralign: u64,
};

const compression_flag = 1 << 11;

/// Takes in a Section header and the full contents of the ELF file and returns
/// a slice of data that represents the section in question. Only allocates in the
/// case that the section contents are compressed.
fn sectionData(scratch: Allocator, class: consts.Class, section: *const Section, contents: String) ParseError!String {
    const start = section.offset;
    const end = section.offset + section.size;

    // safety checks
    if (start == end or start >= contents.len or end > contents.len) {
        log.errf("invalid section bounds at offset: 0x{x}", .{section.offset});
        return error.InvalidELFFile;
    }

    var buf = contents[start..end];

    // if the section is compressed, read the compression header and uncompress the data accordingly
    if (section.flags & compression_flag > 0) {
        var compression_r: Reader = undefined;
        compression_r.init(buf);

        const header = switch (class) {
            .@"32" => CompressedSectionHeader{
                .ch_type = try readCompressedHeaderField(u32, &compression_r),
                .ch_reserved = 0, // not present in 32-bit ELF
                .ch_size = try readCompressedHeaderField(u32, &compression_r),
                .ch_addralign = try readCompressedHeaderField(u32, &compression_r),
            },
            .@"64" => CompressedSectionHeader{
                .ch_type = try readCompressedHeaderField(u32, &compression_r),
                .ch_reserved = try readCompressedHeaderField(u32, &compression_r),
                .ch_size = try readCompressedHeaderField(u64, &compression_r),
                .ch_addralign = try readCompressedHeaderField(u64, &compression_r),
            },
            else => {
                log.errf("invalid ELF class: {any}", .{class});
                return error.InvalidELFFile;
            },
        };

        // advance past the header
        buf = buf[compression_r.offset()..];

        var reader = io.fixedBufferStream(buf);
        var write_arr = ArrayList(u8).init(scratch);
        errdefer write_arr.deinit();

        switch (header.ch_type) {
            1 => compress.zlib.decompress(reader.reader(), write_arr.writer()) catch |err| {
                log.errf("unable to zlib decompress ELF section: {!}", .{err});
                return switch (err) {
                    error.OutOfMemory => |e| e,
                    else => error.InvalidELFFile,
                };
            },
            else => {
                log.errf("unsupported ELF section compression type: {d}", .{header.ch_type});
                return error.InvalidELFFile;
            },
        }

        buf = try write_arr.toOwnedSlice();
    }

    return buf;
}

fn readCompressedHeaderField(comptime T: type, compression_r: *Reader) error{InvalidELFFile}!T {
    return compression_r.read(T) catch |err| {
        log.errf("unable to read compressed ELF header field: {!}", .{err});
        return error.InvalidELFFile;
    };
}

test "sectionData" {
    const contents = "hello";
    var sec = mem.zeroes(Section);

    //
    // Various bounds checks
    //

    sec.offset = 0;
    sec.size = 0;
    try t.expectError(error.InvalidELFFile, sectionData(t.allocator, .@"32", &sec, contents));

    sec.offset = 100;
    try t.expectError(error.InvalidELFFile, sectionData(t.allocator, .@"32", &sec, contents));

    sec.offset = 0;
    sec.size = 100;
    try t.expectError(error.InvalidELFFile, sectionData(t.allocator, .@"32", &sec, contents));

    //
    // OK
    //

    sec.offset = 0;
    sec.size = 3;
    try t.expectEqualSlices(u8, "hel", try sectionData(t.allocator, .@"32", &sec, contents));
}

test "sectionData with zlib compression" {
    const goloop_info = @embedFile("test_files/linux_x86-64_goloop_out_info");
    var section = mem.zeroes(Section);
    section.size = goloop_info.len;
    section.flags = compression_flag;

    const res = try sectionData(t.allocator, .@"64", &section, goloop_info);
    defer t.allocator.free(res);

    // these values were validated externally using `zlib-flate`
    try t.expectEqual(671639, res.len);
    try t.expectEqualSlices(u8, &.{ 0x58, 0x3, 0, 0, 0x4, 0 }, res[0..6]);
}

/// Stores all header data pertaining to the given ELF binary
pub const HeaderData = struct {
    ident: Ident = Ident{},
    file_type: consts.FileType = .none,
    machine: consts.Machine = .none,
    version: consts.Version = .unknown,

    // virtual address at which the start of the program resides
    entry: usize = 0,

    // byte offset from the start of the file at which the program header table is located
    phoff: usize = 0,

    // byte offset from the start of the file at which the section header table is located
    shoff: usize = 0,

    // processor-specific flags, which take the form of EF_<machine_flag>
    flags: u32 = 0,

    // the number of bytes in the header
    header_size: u16 = 0,

    // the number of bytes in one entry in the program header table (all entries are the same size)
    phent_size: u16 = 0,

    // the number of entries in the program header table
    phent_num: u16 = 0,

    // the number of bytes in one entry in the section header table (all entries are the same size)
    shent_size: u16 = 0,

    // the number of entries in the section header table
    shent_num: u16 = 0,

    // the section header table index of the entry associated with the section name string table
    string_table_ndx: u16 = 0,

    // whether or not the binary is a position independent executable
    pie: bool = false,

    // the list of section headers and the data of each section
    sections: ArrayList(Section) = undefined,

    const Self = *@This();

    fn sectionHeaderByName(self: Self, name: String) ?Section {
        for (self.sections.items) |section| {
            if (strings.eql(section.name, name)) {
                return section;
            }
        }

        return null;
    }
};

fn read(r: *Reader, comptime T: type) error{UnexpectedValue}!T {
    return r.read(T) catch |err| {
        log.errf("unable to read " ++ @typeName(T) ++ ": {!}", .{err});
        return error.UnexpectedValue;
    };
}

/// Options for reading ELF files from disk and parsing them
pub const LoadOpts = struct {
    /// Must be an arena's allocator and must be free'd upon error by the caller
    perm: Allocator = undefined,

    /// Must be an arena's allocator and must always be free'd by the caller
    scratch: Allocator = undefined,

    /// the relative or absolute path to the file to parse
    path: String,
};

pub const LoadError = file.MMapError || ParseError || dwarf.ParseError;

/// loads the contents of the binary file at the given path, parses it according to
/// the ELF spec, and maps it in to a platform-independent type. Caller owns returned
/// memory, and it is allocated in the permanent arena.
pub fn load(opts: *const LoadOpts) LoadError!*types.Target {
    const z = trace.zoneN(@src(), "load elf file");
    defer z.end();

    var fp = try file.open(opts.path, .{ .mode = .read_only });
    defer fp.close();

    const contents = try file.mapWholeFile(fp);
    defer file.munmap(contents);

    const data = try parse(opts, contents);

    const target = try opts.perm.create(types.Target);
    target.* = .{
        .flags = .{
            .pie = data.pie,
        },
        .addr_size = .four,
        .unwinder = undefined,
        .compile_units = undefined,
        .strings = try strings.Cache.init(opts.perm),
    };

    const dwarf_sections = try getDWARFSections(opts, contents, data);

    const dwarf_opts = try opts.scratch.create(dwarf.ParseOpts);
    dwarf_opts.* = .{
        .scratch = opts.scratch,
        .sections = dwarf_sections,
    };

    try dwarf.parse(opts.perm, dwarf_opts, target);
    return target;
}

pub const ParseError = error{
    InvalidELFMagic,
    InvalidELFVersion,
    InvalidELFFile,
    OutOfMemory,
    UnexpectedValue,
} || dwarf.ParseError;

fn parse(opts: *const LoadOpts, contents: String) ParseError!*HeaderData {
    const z = trace.zone(@src());
    defer z.end();

    const data = try opts.scratch.create(HeaderData);
    data.* = .{ .sections = ArrayList(Section).init(opts.scratch) };

    if (contents.len < consts.EI_NIDENT) {
        log.err("file is shorter than EI_NIDENT");
        return error.InvalidELFMagic;
    }

    const identBuf = contents[0..consts.EI_NIDENT];

    {
        //
        // Check the magic value
        //

        const magic = identBuf[0..consts.MAGIC.len];
        if (!strings.eql(magic, consts.MAGIC)) {
            log.errf("invalid magic value: {any}", .{magic});
            return error.InvalidELFMagic;
        }
    }

    {
        //
        // Read the e_ident section
        //

        data.ident.class = try safe.enumFromInt(consts.Class, identBuf[consts.EI_CLASS]);
        data.ident.data = try safe.enumFromInt(consts.Data, identBuf[consts.EI_DATA]);
        data.ident.version = try safe.enumFromInt(consts.Version, identBuf[consts.EI_VERSION]);
        data.ident.os_abi = try safe.enumFromInt(consts.OSABI, identBuf[consts.EI_OSABI]);
        data.ident.abi_version = identBuf[consts.EI_ABIVERSION];

        if (data.ident.version != consts.Version.current) {
            log.errf("invalid version: {any}", .{data.ident.version});
            return error.InvalidELFVersion;
        }
    }

    {
        //
        // Read the rest of the e_* header fields
        //

        const header_buf = contents[0..@sizeOf(stdelf.Ehdr)];

        var offset: usize = consts.EI_NIDENT;
        data.file_type = try safe.enumFromInt(consts.FileType, header_buf[offset]);
        offset += @sizeOf(stdelf.ET);

        // @NOTE (jrc): using values not explicitly defined is typically
        // processor-specific, and since we don't know what to do with
        // them, treat that as an error
        data.machine = try safe.enumFromInt(consts.Machine, header_buf[offset]);
        offset += @sizeOf(stdelf.EM);

        data.version = try safe.enumFromInt(consts.Version, header_buf[offset]);
        offset += consts.WORD;

        var r: Reader = undefined;
        r.init(header_buf);
        r.seek(offset);

        data.entry = try read(&r, usize);
        data.phoff = try read(&r, usize);
        data.shoff = try read(&r, usize);
        data.flags = try read(&r, u32);
        data.header_size = try read(&r, u16);
        data.phent_size = try read(&r, u16);
        data.phent_num = try read(&r, u16);
        data.shent_size = try read(&r, u16);
        data.shent_num = try read(&r, u16);
        data.string_table_ndx = try read(&r, u16);
    }

    var section_name_indexes = ArrayList(u32).init(opts.scratch);
    var section_names_offset: usize = 0;
    var section_names_size: usize = 0;

    {
        //
        // Parse section headers
        //

        const addr_size = switch (@sizeOf(usize)) {
            4 => u32,
            8 => u64,
            else => @compileError("expected pointer size of 32 or 64"),
        };

        const start = data.shoff;
        const end = data.shoff + (data.shent_size * data.shent_num);
        const section_header_buf = contents[start..end];

        var ndx: usize = 0;
        while (ndx < data.shent_num) : (ndx += 1) {
            const offset = ndx * data.shent_size;
            var section = Section{};

            var r: Reader = undefined;
            r.init(section_header_buf);
            r.seek(offset);

            const name_ndx = try read(&r, u32);
            try section_name_indexes.append(name_ndx);

            const sectionType = try read(&r, u32);
            section.type = try safe.enumFromInt(consts.SectionType, sectionType);

            section.flags = try read(&r, addr_size);
            section.addr = try read(&r, usize);
            section.offset = try read(&r, usize);
            section.size = try read(&r, addr_size);
            section.link = try read(&r, u32);
            section.extra_info = try read(&r, u32);
            section.addr_align = try read(&r, addr_size);
            section.ent_size = try read(&r, addr_size);

            try data.sections.append(section);

            if (ndx == data.string_table_ndx) {
                section_names_offset = section.offset;
                section_names_size = section.size;
            }
        }
    }

    {
        //
        // Assign section names based on the section's name index
        //

        const start = section_names_offset;
        const end = section_names_offset + section_names_size;
        const section_names_buf = contents[start..end];

        // parse section names
        for (data.sections.items, 0..) |_, section_ndx| {
            const name_ndx = section_name_indexes.items[section_ndx];
            var name = ArrayList(u8).init(opts.scratch);

            const max = 256;
            for (0..max) |char_ndx| {
                const c = section_names_buf[name_ndx + char_ndx];
                // have we reached the end of the name?
                if (c == 0) break;

                try name.append(c);

                assert(char_ndx < max - 1);
            }

            data.sections.items[section_ndx].name = try name.toOwnedSlice();
        }
    }

    {
        //
        // Determine whether or not the binary is a position independent executable. This information is
        // stored in the .dynamic section in the FLAGS_1 field.
        //

        if (data.sectionHeaderByName(".dynamic")) |dynamic| {
            const start = dynamic.offset;
            const end = dynamic.offset + dynamic.size;
            const dynamicBuf = contents[start..end];

            var r: Reader = undefined;
            r.init(dynamicBuf);

            const max = 256;
            for (0..max) |ndx| {
                var tag: u64 = 0;
                var val: u64 = 0;

                if (data.ident.class == .@"32") {
                    const tag32 = try read(&r, u32);
                    tag = @intCast(tag32);

                    const val32 = try read(&r, u32);
                    val = @intCast(val32);
                } else {
                    tag = try read(&r, u64);
                    val = try read(&r, u64);
                }

                if (tag != stdelf.DT_FLAGS_1) {
                    if (r.atEOF()) break;
                    continue;
                }

                const DF_1_PIE = 0x08000000;
                if ((val & DF_1_PIE) > 0) {
                    data.pie = true;
                    break;
                }

                assert(ndx < max - 1);
            }
        }
    }

    return data;
}

/// allocated in scratch
fn getDWARFSections(
    opts: *const LoadOpts,
    contents: String,
    data: *HeaderData,
) ParseError!*dwarf.Sections {
    const sections = try opts.scratch.create(dwarf.Sections);

    sections.* = dwarf.Sections{
        .abbrev = try getRequiredSection(opts.scratch, data, contents, ".debug_abbrev"),
        .line = try getRequiredSection(opts.scratch, data, contents, ".debug_line"),
        .info = try getRequiredSection(opts.scratch, data, contents, ".debug_info"),

        .addr = try getOptionalSection(opts.scratch, data, contents, ".debug_addr"),
        .aranges = try getOptionalSection(opts.scratch, data, contents, ".debug_aranges"),
        .frame = try getOptionalSection(opts.scratch, data, contents, ".debug_frame"),
        .eh_frame = try getOptionalSection(opts.scratch, data, contents, ".eh_frame"),
        .line_str = try getOptionalSection(opts.scratch, data, contents, ".debug_line_str"),
        .loc = try getOptionalSection(opts.scratch, data, contents, ".debug_loc"),
        .loclists = try getOptionalSection(opts.scratch, data, contents, ".debug_loclists"),
        .names = try getOptionalSection(opts.scratch, data, contents, ".debug_names"),
        .macinfo = try getOptionalSection(opts.scratch, data, contents, ".debug_macinfo"),
        .macro = try getOptionalSection(opts.scratch, data, contents, ".debug_macro"),
        .pubnames = try getOptionalSection(opts.scratch, data, contents, ".debug_pubnames"),
        .pubtypes = try getOptionalSection(opts.scratch, data, contents, ".debug_pubtypes"),
        .ranges = try getOptionalSection(opts.scratch, data, contents, ".debug_ranges"),
        .rnglists = try getOptionalSection(opts.scratch, data, contents, ".debug_rnglists"),
        .str = try getOptionalSection(opts.scratch, data, contents, ".debug_str"),
        .str_offsets = try getOptionalSection(opts.scratch, data, contents, ".debug_str_offsets"),
        .types = try getOptionalSection(opts.scratch, data, contents, ".debug_types"),
    };

    return sections;
}

fn getRequiredSection(
    scratch: Allocator,
    data: *HeaderData,
    contents: String,
    name: String,
) ParseError!dwarf.Section {
    const section = data.sectionHeaderByName(name);
    if (section) |s| {
        return .{
            .addr = s.addr,
            .contents = try sectionData(scratch, data.ident.class, &s, contents),
        };
    }

    log.errf("required section not found: {s}", .{name});
    return error.InvalidELFFile;
}

fn getOptionalSection(
    scratch: Allocator,
    data: *HeaderData,
    contents: String,
    name: String,
) ParseError!dwarf.Section {
    const section = data.sectionHeaderByName(name);
    if (section) |s| {
        return .{
            .addr = s.addr,
            .contents = try sectionData(scratch, data.ident.class, &s, contents),
        };
    }

    return .{ .addr = undefined, .contents = "" };
}

const cloop = @embedFile("test_files/linux_x86-64_cloop_out");

test "load OS file errors" {
    var opts = LoadOpts{ .path = "/invalid/path/to/file" };

    // shouldn't allocate
    opts.perm = t.failing_allocator;
    opts.scratch = t.failing_allocator;

    try t.expectError(error.FileNotFound, load(&opts));

    opts.path = "assets/test_files/empty_file";
    try t.expectError(error.FileEmpty, load(&opts));

    opts.path = "assets/test_files/symlink_empty";
    try t.expectError(error.FileEmpty, load(&opts));
}

test "load ELF files" {
    //
    // Loads actual ELF binaries from disk. Before running, they must have been built with:
    //   $ cd assets
    //   $ ./build.sh
    //

    const dw_consts = @import("dwarf/consts.zig");

    const TestCase = struct {
        path: String,
        pie: bool = false,
        cu_lang: dw_consts.Language,
    };

    var cases = ArrayList(TestCase).init(t.allocator);
    defer cases.deinit();
    try cases.appendSlice(&.{
        .{
            .path = "./assets/cbacktrace/out",
            .cu_lang = .DW_LANG_C11,
        },
        .{
            .path = "./assets/cfastloop/out",
            .cu_lang = .DW_LANG_C11,
        },
        .{
            .path = "./assets/cloop/out",
            .cu_lang = .DW_LANG_C11,
        },
        .{
            // should follow symlinks
            .path = "./assets/test_files/symlink_linux_x86-64_cloop_out",
            .cu_lang = .DW_LANG_C11,
        },
        .{
            .path = "./assets/cmulticu/out",
            .cu_lang = .DW_LANG_C11,
        },
        .{
            .path = "./assets/cppsimple/out",
            .cu_lang = .DW_LANG_C_plus_plus_14,
        },
        .{
            .path = "./assets/cppclass/out",
            .cu_lang = .DW_LANG_C_plus_plus_14,
        },
        .{
            .path = "./assets/cprint/out",
            .cu_lang = .DW_LANG_C11,
        },
        .{
            .path = "./assets/cprint/out",
            .cu_lang = .DW_LANG_C11,
        },
        // .{
        //     .path = "./assets/goloop/out",
        //     .cu_lang = .DW_LANG_Go,
        // },
        .{
            .path = "./assets/rustloop/out",
            .pie = true,
            .cu_lang = .DW_LANG_Rust,
        },
        .{
            .path = "./assets/zigloop/out",
            .cu_lang = .DW_LANG_Zig,
        },
    });

    if (!flags.CI) {
        // @NOTE (jrc): we test jai in local builds while the
        // compiler it still in beta
        try cases.append(.{
            .path = "./assets/jailoop/out",
            .cu_lang = .DW_LANG_Jai,
        });

        // @TODO (jrc): fix cinline in CI (it works locally)
        try cases.append(.{
            .path = "./assets/cinline/out",
            .cu_lang = .DW_LANG_C11,
        });

        // There appears to be a bug in odin related to libedit
        // https://github.com/odin-lang/Odin/issues/2271
        try cases.append(.{
            .path = "./assets/odinloop/out",
            .cu_lang = .DW_LANG_Odin,
        });
    }

    for (cases.items, 0..) |case, ndx| {
        defer log.flush();

        log.debugf("load ELF files: {s}", .{case.path});

        var arena = ArenaAllocator.init(t.allocator);
        defer arena.deinit();

        const target = load(&.{
            .perm = arena.allocator(),
            .scratch = arena.allocator(),
            .path = case.path,
        }) catch |err| {
            log.errf("failed to parse {s}: {!}", .{ case.path, err });
            return err;
        };

        try t.expect(case.pie == target.flags.pie);

        const first = target.compile_units[0];
        try t.expectEqual(case.cu_lang.toGeneric() catch unreachable, first.language);

        for (target.compile_units) |cu| {
            // ensure all CU address ranges are sorted
            try t.expect(std.sort.isSorted(
                types.AddressRange,
                cu.ranges,
                {},
                types.AddressRange.sortByLowAddress,
            ));

            // ensure each functions' address ranges are sorted
            try t.expect(std.sort.isSorted(
                types.CompileUnit.Functions.Range,
                cu.functions.ranges,
                {},
                types.CompileUnit.Functions.Range.sortByLowAddress,
            ));
        }

        if (ndx == 0) {
            // Since we have a fully-populated, real types.CompileUnit from disk, this is
            // a reasonable spot to test out memory leak checking in CompileUnit.copyFrom.
            // We only do it on one CU because it's a slow check.
            try t.checkAllAllocationFailures(
                t.allocator,
                findCompileUnitCopyAllocationFailures,
                .{first},
            );
        }
    }
}

fn findCompileUnitCopyAllocationFailures(alloc: Allocator, cu: types.CompileUnit) !void {
    comptime if (!builtin.is_test) @compileError(@src().fn_name ++ " must only be called in tests");

    var arena = ArenaAllocator.init(alloc);
    defer arena.deinit();
    const scratch = arena.allocator();

    var dst = try scratch.create(types.CompileUnit);
    try dst.copyFrom(alloc, cu);

    try t.expectEqual(cu.language, dst.language);

    // upon success, clean up all data that was allocated
    alloc.free(dst.ranges);
    for (dst.sources) |s| alloc.free(s.statements);
    alloc.free(dst.sources);
    alloc.free(dst.data_types);
    alloc.free(dst.variables);
    for (dst.functions.functions) |f| f.deinit(alloc);
    alloc.free(dst.functions.functions);
    alloc.free(dst.functions.ranges);
}

test "parse cloop" {
    assert(cloop.len > 0);

    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();

    const opts = LoadOpts{
        .perm = t.failing_allocator, // perm should never allocate
        .scratch = arena.allocator(),
        .path = undefined,
    };

    //
    // Check all the mapped data, comparing against what readelf,
    // objdump, and pyelftools report about the binary's contents
    //

    const data = try parse(&opts, cloop);

    // Ident headers
    try t.expectEqual(consts.Class.@"64", data.ident.class);
    try t.expectEqual(consts.Data.@"2lsb", data.ident.data);
    try t.expectEqual(consts.Version.current, data.ident.version);
    try t.expectEqual(consts.OSABI.sysv, data.ident.os_abi);
    try t.expectEqual(@as(u8, 0), data.ident.abi_version);

    // other header data
    try t.expectEqual(consts.FileType.executable, data.file_type);
    try t.expectEqual(consts.Machine.x86_64, data.machine);
    try t.expectEqual(consts.Version.current, data.version);
    try t.expectEqual(@as(usize, 0x401070), data.entry);
    try t.expectEqual(@as(usize, 64), data.phoff);
    try t.expectEqual(@as(usize, 23920), data.shoff);
    try t.expectEqual(@as(usize, 0), data.flags);
    try t.expectEqual(@as(usize, 64), data.header_size);
    try t.expectEqual(@as(usize, 56), data.phent_size);
    try t.expectEqual(@as(usize, 13), data.phent_num);
    try t.expectEqual(@as(usize, 64), data.shent_size);
    try t.expectEqual(@as(usize, 37), data.shent_num);
    try t.expectEqual(@as(usize, 36), data.string_table_ndx);
    try t.expectEqual(false, data.pie);

    //
    // Check that section header data parses correctly
    //

    try t.expectEqual(@as(usize, data.shent_num), data.sections.items.len);

    const section_data = [_]Section{
        .{ .name = "", .type = @enumFromInt(0x0), .flags = 0x0, .addr = 0x0, .offset = 0x0, .size = 0x0, .link = 0, .extra_info = 0, .addr_align = 0, .ent_size = 0 },
        .{ .name = ".interp", .type = @enumFromInt(0x1), .flags = 0x2, .addr = 0x400318, .offset = 0x318, .size = 0x1c, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".note.gnu.property", .type = @enumFromInt(0x7), .flags = 0x2, .addr = 0x400338, .offset = 0x338, .size = 0x40, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".note.gnu.build-id", .type = @enumFromInt(0x7), .flags = 0x2, .addr = 0x400378, .offset = 0x378, .size = 0x24, .link = 0x0, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".note.ABI-tag", .type = @enumFromInt(0x7), .flags = 0x2, .addr = 0x40039c, .offset = 0x39c, .size = 0x20, .link = 0x0, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".gnu.hash", .type = @enumFromInt(0x6ffffff6), .flags = 0x2, .addr = 0x4003c0, .offset = 0x3c0, .size = 0x24, .link = 0x6, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".dynsym", .type = @enumFromInt(0xb), .flags = 0x2, .addr = 0x4003e8, .offset = 0x3e8, .size = 0xc0, .link = 0x7, .extra_info = 0x1, .addr_align = 0x8, .ent_size = 0x18 },
        .{ .name = ".dynstr", .type = @enumFromInt(0x3), .flags = 0x2, .addr = 0x4004a8, .offset = 0x4a8, .size = 0x65, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".gnu.version", .type = @enumFromInt(0x6fffffff), .flags = 0x2, .addr = 0x40050e, .offset = 0x50e, .size = 0x10, .link = 0x6, .extra_info = 0x0, .addr_align = 0x2, .ent_size = 0x2 },
        .{ .name = ".gnu.version_r", .type = @enumFromInt(0x6ffffffe), .flags = 0x2, .addr = 0x400520, .offset = 0x520, .size = 0x30, .link = 0x7, .extra_info = 0x1, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".rela.dyn", .type = @enumFromInt(0x4), .flags = 0x2, .addr = 0x400550, .offset = 0x550, .size = 0x48, .link = 0x6, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x18 },
        .{ .name = ".rela.plt", .type = @enumFromInt(0x4), .flags = 0x42, .addr = 0x400598, .offset = 0x598, .size = 0x60, .link = 0x6, .extra_info = 0x17, .addr_align = 0x8, .ent_size = 0x18 },
        .{ .name = ".init", .type = @enumFromInt(0x1), .flags = 0x6, .addr = 0x401000, .offset = 0x1000, .size = 0x1b, .link = 0x0, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".plt", .type = @enumFromInt(0x1), .flags = 0x6, .addr = 0x401020, .offset = 0x1020, .size = 0x50, .link = 0x0, .extra_info = 0x0, .addr_align = 0x10, .ent_size = 0x10 },
        .{ .name = ".text", .type = @enumFromInt(0x1), .flags = 0x6, .addr = 0x401070, .offset = 0x1070, .size = 0x13b, .link = 0x0, .extra_info = 0x0, .addr_align = 0x10, .ent_size = 0x0 },
        .{ .name = ".fini", .type = @enumFromInt(0x1), .flags = 0x6, .addr = 0x4011ac, .offset = 0x11ac, .size = 0xd, .link = 0x0, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".rodata", .type = @enumFromInt(0x1), .flags = 0x2, .addr = 0x402000, .offset = 0x2000, .size = 0x2a, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".eh_frame_hdr", .type = @enumFromInt(0x1), .flags = 0x2, .addr = 0x40202c, .offset = 0x202c, .size = 0x2c, .link = 0x0, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".eh_frame", .type = @enumFromInt(0x1), .flags = 0x2, .addr = 0x402058, .offset = 0x2058, .size = 0x88, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".init_array", .type = @enumFromInt(0xe), .flags = 0x3, .addr = 0x403df8, .offset = 0x2df8, .size = 0x8, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x8 },
        .{ .name = ".fini_array", .type = @enumFromInt(0xf), .flags = 0x3, .addr = 0x403e00, .offset = 0x2e00, .size = 0x8, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x8 },
        .{ .name = ".dynamic", .type = @enumFromInt(0x6), .flags = 0x3, .addr = 0x403e08, .offset = 0x2e08, .size = 0x1d0, .link = 0x7, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x10 },
        .{ .name = ".got", .type = @enumFromInt(0x1), .flags = 0x3, .addr = 0x403fd8, .offset = 0x2fd8, .size = 0x10, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x8 },
        .{ .name = ".got.plt", .type = @enumFromInt(0x1), .flags = 0x3, .addr = 0x403fe8, .offset = 0x2fe8, .size = 0x38, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x8 },
        .{ .name = ".data", .type = @enumFromInt(0x1), .flags = 0x3, .addr = 0x404020, .offset = 0x3020, .size = 0x4, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".bss", .type = @enumFromInt(0x8), .flags = 0x3, .addr = 0x404028, .offset = 0x3024, .size = 0x10, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".comment", .type = @enumFromInt(0x1), .flags = 0x30, .addr = 0x0, .offset = 0x3024, .size = 0x5c, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x1 },
        .{ .name = ".gnu.build.attributes", .type = @enumFromInt(0x7), .flags = 0x80, .addr = 0x406038, .offset = 0x3080, .size = 0x1724, .link = 0xe, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".debug_aranges", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x47a4, .size = 0x30, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_info", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x47d4, .size = 0x32b, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_abbrev", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x4aff, .size = 0x13a, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_line", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x4c39, .size = 0x8d, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_str", .type = @enumFromInt(0x1), .flags = 0x30, .addr = 0x0, .offset = 0x4cc6, .size = 0x251, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x1 },
        .{ .name = ".debug_line_str", .type = @enumFromInt(0x1), .flags = 0x30, .addr = 0x0, .offset = 0x4f17, .size = 0xd9, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x1 },
        .{ .name = ".symtab", .type = @enumFromInt(0x2), .flags = 0x0, .addr = 0x0, .offset = 0x4ff0, .size = 0x690, .link = 0x23, .extra_info = 0x32, .addr_align = 0x8, .ent_size = 0x18 },
        .{ .name = ".strtab", .type = @enumFromInt(0x3), .flags = 0x0, .addr = 0x0, .offset = 0x5680, .size = 0x56d, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".shstrtab", .type = @enumFromInt(0x3), .flags = 0x0, .addr = 0x0, .offset = 0x5bed, .size = 0x17c, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
    };

    for (section_data, 0..) |expected, ndx| {
        const section = data.sections.items[ndx];

        try t.expectEqualSlices(u8, expected.name, section.name);
        try t.expectEqual(expected.type, section.type);
        try t.expectEqual(expected.flags, section.flags);
        try t.expectEqual(expected.addr, section.addr);
        try t.expectEqual(expected.offset, section.offset);

        try t.expectEqual(expected.size, section.size);
        try t.expectEqual(expected.link, section.link);
        try t.expectEqual(expected.extra_info, section.extra_info);
        try t.expectEqual(expected.addr_align, section.addr_align);
        try t.expectEqual(expected.ent_size, section.ent_size);
    }

    //
    // Spot check the contents of a few sections
    //

    {
        const interp = data.sectionHeaderByName(".interp");
        const contents = try sectionData(t.allocator, .@"64", &interp.?, cloop);
        try t.expectEqualSlices(u8, "/lib64/ld-linux-x86-64.so.2\x00", contents);
    }

    {
        const text = data.sectionHeaderByName(".text");
        const contents = try sectionData(t.allocator, .@"64", &text.?, cloop);
        try t.expectEqual(@as(usize, 0x13b), contents.len);

        // spot check the beginning and the end of the section
        try t.expectEqual(@as(u8, 0xf3), contents[0]);
        try t.expectEqual(@as(u8, 0x0f), contents[1]);
        try t.expectEqual(@as(u8, 0xeb), contents[0x139]);
        try t.expectEqual(@as(u8, 0xc3), contents[0x13a]);
    }

    {
        // check that all expected debug info sections load correctly
        const sec = try getDWARFSections(&opts, cloop, data);

        try t.expectEqual(@as(usize, 0x30), sec.aranges.contents.len);
        try t.expectEqual(@as(u8, 0x2c), sec.aranges.contents[0]);
        try t.expectEqual(@as(u8, 0x00), sec.aranges.contents[sec.aranges.contents.len - 1]);

        try t.expectEqual(@as(usize, 0x32b), sec.info.contents.len);
        try t.expectEqual(@as(u8, 0x27), sec.info.contents[0]);
        try t.expectEqual(@as(u8, 0x00), sec.info.contents[sec.info.contents.len - 1]);

        try t.expectEqual(@as(usize, 0x13a), sec.abbrev.contents.len);
        try t.expectEqual(@as(u8, 0x01), sec.abbrev.contents[0]);
        try t.expectEqual(@as(u8, 0x00), sec.abbrev.contents[sec.abbrev.contents.len - 1]);

        try t.expectEqual(@as(usize, 0x8d), sec.line.contents.len);
        try t.expectEqual(@as(u8, 0x89), sec.line.contents[0]);
        try t.expectEqual(@as(u8, 0x01), sec.line.contents[sec.line.contents.len - 1]);

        try t.expectEqual(@as(usize, 0x251), sec.str.contents.len);
        try t.expect(mem.startsWith(u8, sec.str.contents, "getpid"));
        try t.expectEqual(@as(u8, 0x00), sec.str.contents[sec.str.contents.len - 1]);

        try t.expectEqual(@as(usize, 0xd9), sec.line_str.contents.len);
        try t.expect(mem.startsWith(u8, sec.line_str.contents, "main.c"));
        try t.expectEqual(@as(u8, 0x00), sec.line_str.contents[sec.line_str.contents.len - 1]);

        // the following optional sections are not set
        try t.expectEqual(@as(usize, 0), sec.loc.contents.len);
        try t.expectEqual(@as(usize, 0), sec.ranges.contents.len);
        try t.expectEqual(@as(usize, 0), sec.rnglists.contents.len);
    }
}

test "parse zigloop" {
    const zigloop = @embedFile("test_files/linux_x86-64_zigloop_out");
    assert(zigloop.len > 0);

    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();

    const opts = LoadOpts{
        .perm = t.failing_allocator, // perm should never allocate
        .scratch = arena.allocator(),
        .path = undefined,
    };

    //
    // Check all the mapped data, comparing against what readelf,
    // objdump, and pyelftools report about the binary's contents
    //

    const data = try parse(&opts, zigloop);

    // Ident headers
    try t.expectEqual(consts.Class.@"64", data.ident.class);
    try t.expectEqual(consts.Data.@"2lsb", data.ident.data);
    try t.expectEqual(consts.Version.current, data.ident.version);
    try t.expectEqual(consts.OSABI.sysv, data.ident.os_abi);
    try t.expectEqual(@as(u8, 0), data.ident.abi_version);

    // other header data
    try t.expectEqual(consts.FileType.executable, data.file_type);
    try t.expectEqual(consts.Machine.x86_64, data.machine);
    try t.expectEqual(consts.Version.current, data.version);
    try t.expectEqual(@as(usize, 0x10334f0), data.entry);
    try t.expectEqual(@as(usize, 64), data.phoff);
    try t.expectEqual(@as(usize, 2217128), data.shoff);
    try t.expectEqual(@as(usize, 0), data.flags);
    try t.expectEqual(@as(usize, 64), data.header_size);
    try t.expectEqual(@as(usize, 56), data.phent_size);
    try t.expectEqual(@as(usize, 9), data.phent_num);
    try t.expectEqual(@as(usize, 64), data.shent_size);
    try t.expectEqual(@as(usize, 21), data.shent_num);
    try t.expectEqual(@as(usize, 19), data.string_table_ndx);
    try t.expectEqual(false, data.pie);

    try t.expectEqual(@as(usize, data.shent_num), data.sections.items.len);

    //
    // Check that section header data parses correctly
    //

    const section_data = [_]Section{
        .{ .name = "", .type = @enumFromInt(0x0), .flags = 0x0, .addr = 0x0, .offset = 0x0, .size = 0x0, .link = 0, .extra_info = 0, .addr_align = 0, .ent_size = 0 },
        .{ .name = ".rodata", .type = @enumFromInt(0x1), .flags = 0x32, .addr = 0x1000240, .offset = 0x240, .size = 0x207b8, .link = 0x0, .extra_info = 0x0, .addr_align = 0x20, .ent_size = 0x0 },
        .{ .name = ".eh_frame_hdr", .type = @enumFromInt(0x1), .flags = 0x2, .addr = 0x10209f8, .offset = 0x209f8, .size = 0x29a4, .link = 0x0, .extra_info = 0x0, .addr_align = 0x4, .ent_size = 0x0 },
        .{ .name = ".eh_frame", .type = @enumFromInt(0x1), .flags = 0x2, .addr = 0x10233a0, .offset = 0x233a0, .size = 0xf144, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".text", .type = @enumFromInt(0x1), .flags = 0x6, .addr = 0x10334f0, .offset = 0x324f0, .size = 0xa145a, .link = 0x0, .extra_info = 0x0, .addr_align = 0x10, .ent_size = 0x0 },
        .{ .name = ".tbss", .type = @enumFromInt(0x8), .flags = 0x403, .addr = 0x10d4950, .offset = 0xd3950, .size = 0x10, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".got", .type = @enumFromInt(0x1), .flags = 0x3, .addr = 0x10d5950, .offset = 0xd3950, .size = 0x8, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".data", .type = @enumFromInt(0x1), .flags = 0x3, .addr = 0x10d6958, .offset = 0xd3958, .size = 0x9c8, .link = 0x0, .extra_info = 0x0, .addr_align = 0x8, .ent_size = 0x0 },
        .{ .name = ".bss", .type = @enumFromInt(0x8), .flags = 0x3, .addr = 0x10d8000, .offset = 0xd4320, .size = 0x4240, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1000, .ent_size = 0x0 },
        .{ .name = ".debug_loc", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0xd4320, .size = 0x612dd, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_abbrev", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x1355fd, .size = 0x7fb, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_info", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x135df8, .size = 0x4c804, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_ranges", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x1825fc, .size = 0x120f0, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_str", .type = @enumFromInt(0x1), .flags = 0x30, .addr = 0x0, .offset = 0x1946ec, .size = 0x236ae, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x1 },
        .{ .name = ".debug_pubnames", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x1b7d9a, .size = 0xa9c2, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_pubtypes", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x1c275c, .size = 0x5d27, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".debug_line", .type = @enumFromInt(0x1), .flags = 0x0, .addr = 0x0, .offset = 0x1c8483, .size = 0x3cb82, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".comment", .type = @enumFromInt(0x1), .flags = 0x30, .addr = 0x0, .offset = 0x205005, .size = 0x13, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x1 },
        .{ .name = ".symtab", .type = @enumFromInt(0x2), .flags = 0x0, .addr = 0x0, .offset = 0x205018, .size = 0x9bd0, .link = 0x14, .extra_info = 0x4cb, .addr_align = 0x8, .ent_size = 0x18 },
        .{ .name = ".shstrtab", .type = @enumFromInt(0x3), .flags = 0x0, .addr = 0x0, .offset = 0x20ebe8, .size = 0xca, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
        .{ .name = ".strtab", .type = @enumFromInt(0x3), .flags = 0x0, .addr = 0x0, .offset = 0x20ecb2, .size = 0xe7f0, .link = 0x0, .extra_info = 0x0, .addr_align = 0x1, .ent_size = 0x0 },
    };

    for (section_data, 0..) |expected, ndx| {
        const section = data.sections.items[ndx];

        try t.expectEqualSlices(u8, expected.name, section.name);
        try t.expectEqual(expected.type, section.type);
        try t.expectEqual(expected.flags, section.flags);
        try t.expectEqual(expected.addr, section.addr);
        try t.expectEqual(expected.offset, section.offset);

        try t.expectEqual(expected.size, section.size);
        try t.expectEqual(expected.link, section.link);
        try t.expectEqual(expected.extra_info, section.extra_info);
        try t.expectEqual(expected.addr_align, section.addr_align);
        try t.expectEqual(expected.ent_size, section.ent_size);
    }

    //
    // Spot check the contents of a few sections
    //

    {
        const comment = data.sectionHeaderByName(".comment");
        const contents = try sectionData(t.allocator, .@"64", &comment.?, zigloop);
        try t.expectEqualSlices(u8, "Linker: LLD 17.0.6\x00", contents);
    }

    {
        const rodata = data.sectionHeaderByName(".rodata");
        const contents = try sectionData(t.allocator, .@"64", &rodata.?, zigloop);
        try t.expectEqual(@as(usize, 0x207b8), contents.len);

        // spot check the beginning and the end of the section
        try t.expectEqual(@as(u8, 0xd2), contents[0]);
        try t.expectEqual(@as(u8, 0x57), contents[1]);
        try t.expectEqual(@as(u8, 0x69), contents[contents.len - 2]);
        try t.expectEqual(@as(u8, 0x35), contents[contents.len - 1]);
    }
}
