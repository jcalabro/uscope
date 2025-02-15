//! Contains code for safely parsing DWARF frame tables from binaries

const std = @import("std");
const ArenaAllocator = std.heap.ArenaAllocator;
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const mem = std.mem;
const pow = std.math.pow;
const t = std.testing;

const consts = @import("consts.zig");
const dwarf = @import("../dwarf.zig");
const file = @import("../../file.zig");
const info = @import("info.zig");
const logging = @import("../../logging.zig");
const Reader = @import("../../Reader.zig");
const safe = @import("../../safe.zig");
const trace = @import("../../trace.zig");
const types = @import("../../types.zig");

const log = logging.Logger.init(logging.Region.Symbols);

/// A Common Information Entry holds information shared among many FDEs
pub const CIE = struct {
    const Self = @This();

    /// Whether or not this CIE is being parsed from the .eh_frame ELF section, or the .debug_frame DWARF section
    is_eh_frame: bool = true,

    /// Whether or not the CIE's target uses 32 or 64 bit address space
    is_32_bit: bool = true,

    /// A unique identifier for this CIE
    id: u64 = undefined,

    /// A binary may emit DIEs that are of a higher version than the CIE. So a compile unit may emit
    /// DIEs that use DWARF v5, but the call frame may use DWARF v1 in the same binary.
    version: dwarf.Version = undefined,

    /// The raw list of modifiers that must be applied when using this CIE to unwind the stack
    augmentation_data: []const u8 = "",

    /// Added in DWARF v4
    addr_size: types.AddressSize = .four,
    /// Added in DWARF v4
    segment_selector_size: u8 = 0,

    /// A constant that is factored out of all "advance location" instructions
    code_alignment_factor: u64 = 0,
    /// A constant that is factored out of all "offset" instructions
    data_alignment_factor: i64 = 0,

    /// The index of the virtual unwind register that contains the frame's return address
    return_address_register: u64 = 0,

    /// Supplied by the presence of the 'S' field
    /// "When unwinding the stack, signal stack frames are handled slightly differently: the instruction
    /// pointer is assumed to be before the next instruction to execute rather than after it"
    stack_frame_for_invocation_of_signal_handler: bool = false,

    /// Supplied by the 'L' augmentation field
    cie_addr_encoding: consts.ExceptionHeaderFormat = .DW_EH_PE_omit,
    /// Supplied by the 'P' augmentation field
    cie_addr_usage: consts.ExceptionHeaderApplication = .DW_EH_PE_omit,

    /// Supplied by the 'R' augmentation field
    fde_addr_encoding: consts.ExceptionHeaderFormat = .DW_EH_PE_omit,
    /// Supplied by the 'R' augmentation field
    fde_addr_usage: consts.ExceptionHeaderApplication = .DW_EH_PE_omit,

    /// A sequence of rules that are interpreted to create the initial setting of each column in the table
    initial_instructions: []const u8 = "",

    /// The list of FDEs that share this CIE
    fdes: []const FDE = &.{},

    fn free(self: *Self, alloc: Allocator) void {
        for (self.fdes) |fde| alloc.free(fde.instructions);
        alloc.free(self.fdes);

        alloc.free(self.augmentation_data);
        alloc.free(self.initial_instructions);
    }

    fn copyFrom(dest: *Self, alloc: Allocator, src: *const Self) Allocator.Error!void {
        dest.* = src.*;

        dest.augmentation_data = try safe.copySlice(u8, alloc, src.augmentation_data);
        errdefer alloc.free(dest.augmentation_data);

        dest.initial_instructions = try safe.copySlice(u8, alloc, src.initial_instructions);
        errdefer alloc.free(dest.initial_instructions);

        const fdes = try alloc.alloc(FDE, src.fdes.len);
        errdefer {
            for (fdes) |fde| alloc.free(fde.instructions);
            alloc.free(fdes);
        }
        for (src.fdes, 0..) |*fde, ndx| {
            try fdes[ndx].copyFrom(alloc, fde);
        }
        dest.fdes = fdes;
    }
};

/// A Frame Description Entry instructions a running appliation on how to unwind the callstack when the
/// program is stopped at a particular address
pub const FDE = struct {
    const Self = @This();

    cie_offset: u64 = undefined,
    addr_range: types.AddressRange = undefined,
    instructions: []const u8 = undefined,

    fn copyFrom(dest: *Self, alloc: Allocator, src: *const Self) Allocator.Error!void {
        dest.* = src.*;

        dest.instructions = try safe.copySlice(u8, alloc, src.instructions);
        errdefer alloc.free(dest.instructions);
    }
};

pub const CIEList = struct {
    cies: []CIE,

    /// @PERFORMANCE (jrc)
    pub fn findForAddr(self: @This(), addr: types.Address) ?*const CIE {
        for (self.cies) |*cie| {
            for (cie.fdes) |fde| {
                if (fde.addr_range.contains(addr)) return cie;
            }
        }

        return null;
    }
};

/// Loads the frame table from either the .eh_frame section or the .debug_frame section so we can
/// later unwind call stacks while the subordinate is running. The returned CIE is allocated in
/// the scratch arena.
pub fn loadTable(perm_alloc: Allocator, opts: *const dwarf.ParseOpts) dwarf.ParseError![]CIE {
    const z = trace.zoneN(@src(), "load frame table");
    defer z.end();

    var cies = ArrayList(CIE).init(opts.scratch);

    if (opts.sections.eh_frame.contents.len > 0) {
        try loadCIE(opts, true, &cies);
    } else if (opts.sections.frame.contents.len > 0) {
        try loadCIE(opts, false, &cies);
    } else {
        log.err("no data in either frame table section");
        return error.InvalidDWARFInfo;
    }

    // copy to permanent storage and return
    var res = ArrayList(CIE).init(perm_alloc);
    errdefer {
        for (res.items) |*cie| cie.free(perm_alloc);
        res.deinit();
    }
    for (cies.items) |cie| {
        var copy = CIE{};
        try copy.copyFrom(perm_alloc, &cie);
        try res.append(copy);
    }

    return try res.toOwnedSlice();
}

fn loadCIE(opts: *const dwarf.ParseOpts, eh_frame: bool, cies: *ArrayList(CIE)) dwarf.ParseError!void {
    const z = trace.zone(@src());
    defer z.end();

    const contents = switch (eh_frame) {
        true => opts.sections.eh_frame.contents,
        false => opts.sections.frame.contents,
    };

    var frames_r: Reader = undefined;
    frames_r.init(contents);

    while (!frames_r.atEOF()) {
        const cie = try readOneCIE(opts, eh_frame, contents, &frames_r);
        try cies.append(cie.*);
    }
}

fn readOneCIE(opts: *const dwarf.ParseOpts, eh_frame: bool, contents: []const u8, frames_r: *Reader) dwarf.ParseError!*CIE {
    const cie = try opts.scratch.create(CIE);
    cie.* = .{ .is_eh_frame = eh_frame };

    // read the length of the CIE and create a local reader for easier bookkeeping
    const len = try dwarf.readInitialLength(frames_r);
    const end = len + frames_r.offset();
    const is_32_bit = frames_r.offset() == 4;

    var cie_r: Reader = undefined;
    cie_r.init(contents[frames_r.offset()..end]);
    frames_r.advanceBy(len);

    cie.id = switch (is_32_bit) {
        true => try dwarf.read(&cie_r, u32),
        false => try dwarf.read(&cie_r, u64),
    };

    cie.version = try dwarf.readEnum(&cie_r, u8, dwarf.Version);

    cie.augmentation_data = try dwarf.readUntil(&cie_r, 0);

    if (cie.version.isAtLeast(.four)) {
        // two new fields added since DWARF v4
        cie.addr_size = try dwarf.readEnum(&cie_r, u8, types.AddressSize);
        cie.segment_selector_size = try dwarf.read(&cie_r, u8);
    }

    cie.code_alignment_factor = try dwarf.readULEB128(&cie_r);
    cie.data_alignment_factor = try dwarf.readSLEB128(&cie_r);

    if (cie.version.isAtLeast(.three)) {
        // changed from a u8 to a ULEB as of DWARF v3
        cie.return_address_register = try dwarf.readULEB128(&cie_r);
    } else {
        cie.return_address_register = try dwarf.read(&cie_r, u8);
    }

    if (cie.augmentation_data.len > 0) {
        // @REF: https://www.airs.com/blog/archives/460
        for (cie.augmentation_data) |modifier| {
            switch (modifier) {
                // the length of the augmentation data, which we need to read, but can skip
                'z' => _ = try dwarf.readULEB128(&cie_r),

                'S' => cie.stack_frame_for_invocation_of_signal_handler = true,

                // Language Specific Data Area
                'L' => {
                    const mod = try dwarf.read(&cie_r, u8);
                    cie.cie_addr_encoding = try dwarf.safeEnumFromInt(consts.ExceptionHeaderFormat, mod);
                },

                // CIE personality
                'P' => {
                    const mod = try dwarf.read(&cie_r, u8);
                    cie.cie_addr_usage = try dwarf.safeEnumFromInt(consts.ExceptionHeaderApplication, mod);
                },

                // FDE encoding
                'R' => {
                    const mod = try dwarf.read(&cie_r, u8);

                    // lower four bits indicate the encoding of the data
                    cie.fde_addr_encoding = try dwarf.safeEnumFromInt(consts.ExceptionHeaderFormat, mod & 0x0f);

                    // upper four bits indicate how to apply the data
                    cie.fde_addr_usage = try dwarf.safeEnumFromInt(consts.ExceptionHeaderApplication, mod & 0xf0);
                },

                else => {
                    log.errf("unknown frame augmentation data: {c}", .{modifier});
                    return error.InvalidDWARFInfo;
                },
            }
        }
    }

    {
        const initial_instructions = try opts.scratch.alloc(u8, len - cie_r.offset());
        _ = try dwarf.readBuf(&cie_r, initial_instructions);
        cie.initial_instructions = initial_instructions;
    }

    {
        var fdes = ArrayList(FDE).init(opts.scratch);

        const max = pow(usize, 2, 20);
        for (0..max) |fde_ndx| {
            defer assert(fde_ndx < max - 1);

            var done = false;
            var fde = FDE{};
            loadFDE(opts, frames_r, contents, cie, &fde) catch |err| switch (err) {
                error.EndOfFile => done = true,
                else => |e| return e,
            };
            if (done) break;

            try fdes.append(fde);
        }

        cie.fdes = try fdes.toOwnedSlice();
    }

    return cie;
}

const ParseFDEError = dwarf.ParseError || error{EndOfFile};

fn loadFDE(
    opts: *const dwarf.ParseOpts,
    frames_r: *Reader,
    contents: []const u8,
    cie: *const CIE,
    fde: *FDE,
) ParseFDEError!void {
    const z = trace.zone(@src());
    defer z.end();

    // read the length of the FDE and create a local reader for easier bookkeeping
    const start_offset = frames_r.offset();
    const len = try readInitialLength(frames_r);
    if (len == 0) return error.EndOfFile;

    const is_32_bit = frames_r.offset() - start_offset == 4;
    const end = len + frames_r.offset();

    var fde_r: Reader = undefined;
    fde_r.init(contents[frames_r.offset()..end]);
    frames_r.advanceBy(len);

    fde.cie_offset = switch (is_32_bit) {
        true => try dwarf.read(&fde_r, u32),
        false => try dwarf.read(&fde_r, u64),
    };

    if (cie.is_eh_frame) {
        try parseEHFrameFDE(opts, cie, &fde_r, fde);
    } else {
        try parseDebugFrameFDE(opts, cie, &fde_r, fde);
    }

    {
        // the FDE's instructions are a byte array that is the remainder of the length
        const instructions = try opts.scratch.alloc(u8, len - fde_r.offset());
        _ = try dwarf.readBuf(&fde_r, instructions);
        fde.instructions = instructions;
    }
}

/// Parses fields specific to the .eh_frame section
fn parseEHFrameFDE(
    opts: *const dwarf.ParseOpts,
    cie: *const CIE,
    fde_r: *Reader,
    fde: *FDE,
) ParseFDEError!void {
    const addr_size: u8 = if (cie.is_32_bit) 4 else 8;

    const start_addr = try readEHFrameAddr(cie, fde_r);
    const low_base = dwarf.applyOffset(opts.sections.eh_frame.addr, start_addr);
    fde.addr_range.low = types.Address.from(dwarf.applyOffset(low_base, fde.cie_offset + addr_size));

    const num_bytes = try readEHFrameAddr(cie, fde_r);
    fde.addr_range.high = types.Address.from(dwarf.applyOffset(fde.addr_range.low.int(), num_bytes));
}

/// Parses fields specific to the .debug_frame section
fn parseDebugFrameFDE(
    opts: *const dwarf.ParseOpts,
    cie: *const CIE,
    fde_r: *Reader,
    fde: *FDE,
) ParseFDEError!void {
    // @NOTE (jrc): segment selector is not currently in use, so we just discard it for now
    if (cie.segment_selector_size > 0) {
        const segment_selector_buf = try opts.scratch.alloc(u8, cie.segment_selector_size);
        const n = try dwarf.readBuf(fde_r, segment_selector_buf);
        if (n != cie.segment_selector_size) {
            log.err("invalid segment selector");
            return error.InvalidDWARFInfo;
        }
    }

    //
    // @QUESTION (jrc): should these really be u64s? This seems wrong and not portable...
    //

    fde.addr_range.low = types.Address.from(try dwarf.read(fde_r, u64));

    const high_offset = try dwarf.read(fde_r, u64);
    fde.addr_range.high = types.Address.from(dwarf.applyOffset(fde.addr_range.low.int(), high_offset));
}

fn readEHFrameAddr(cie: *const CIE, fde_r: *Reader) ParseFDEError!i128 {
    return switch (cie.fde_addr_encoding) {
        .DW_EH_PE_omit => 0, // noop

        .DW_EH_PE_uleb128 => try dwarf.readULEB128(fde_r),
        .DW_EH_PE_udata2 => try dwarf.read(fde_r, u16),
        .DW_EH_PE_udata4 => try dwarf.read(fde_r, u32),
        .DW_EH_PE_udata8 => try dwarf.read(fde_r, u64),

        .DW_EH_PE_sleb128 => try dwarf.readSLEB128(fde_r),
        .DW_EH_PE_sdata2 => try dwarf.read(fde_r, i16),
        .DW_EH_PE_sdata4 => try dwarf.read(fde_r, i32),
        .DW_EH_PE_sdata8 => try dwarf.read(fde_r, i64),
    };
}

/// Same as dwarf.readInitialLength, but the error set adds EOF and does not log in case of EOF
fn readInitialLength(r: *Reader) ParseFDEError!u64 {
    const val = r.read(u32) catch |err| switch (err) {
        error.EndOfFile => |e| return e,
        else => {
            log.errf("unable to read initial length of FDE: {!}", .{err});
            return error.InvalidDWARFInfo;
        },
    };

    if (val == 0xffffffff) {
        return r.read(u64) catch |err| switch (err) {
            error.EndOfFile => |e| return e,
            else => {
                log.errf("unable to read 64-bit initial length of FDE: {!}", .{err});
                return error.InvalidDWARFInfo;
            },
        };
    }

    return val;
}

test "parse cloop frame table" {
    var sections = mem.zeroes(dwarf.Sections);
    sections.eh_frame.contents = @embedFile("../test_files/linux_x86-64_cloop_out_frame");
    sections.eh_frame.addr = 0x402058;

    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();

    const cies = try loadTable(arena.allocator(), &.{
        .scratch = arena.allocator(),
        .sections = &sections,
        .file_cache = try file.Cache.init(arena.allocator()),
    });

    try t.expectEqual(1, cies.len);
    const cie = cies[0];

    try t.expectEqual(0, cie.id);
    try t.expect(cie.is_32_bit);
    try t.expectEqual(dwarf.Version.one, cie.version);
    try t.expectEqualStrings("zR", cie.augmentation_data);
    try t.expectEqual(types.AddressSize.four, cie.addr_size);
    try t.expectEqual(0, cie.segment_selector_size);
    try t.expectEqual(1, cie.code_alignment_factor);
    try t.expectEqual(-8, cie.data_alignment_factor);
    try t.expectEqual(16, cie.return_address_register);
    try t.expectEqual(false, cie.stack_frame_for_invocation_of_signal_handler);
    try t.expectEqual(consts.ExceptionHeaderFormat.DW_EH_PE_omit, cie.cie_addr_encoding);
    try t.expectEqual(consts.ExceptionHeaderApplication.DW_EH_PE_omit, cie.cie_addr_usage);
    try t.expectEqual(consts.ExceptionHeaderFormat.DW_EH_PE_sdata4, cie.fde_addr_encoding);
    try t.expectEqual(consts.ExceptionHeaderApplication.DW_EH_PE_pcrel, cie.fde_addr_usage);

    try t.expectEqual(4, cie.fdes.len);

    {
        const fde = cie.fdes[0];
        try t.expectEqual(0x1c, fde.cie_offset);
        try t.expectEqual(types.Address.from(0x401070), fde.addr_range.low);
        try t.expectEqual(types.Address.from(0x401096), fde.addr_range.high);
    }

    {
        const fde = cie.fdes[1];
        try t.expectEqual(0x30, fde.cie_offset);
        try t.expectEqual(types.Address.from(0x4010a0), fde.addr_range.low);
        try t.expectEqual(types.Address.from(0x4010a5), fde.addr_range.high);
    }

    {
        const fde = cie.fdes[2];
        try t.expectEqual(0x44, fde.cie_offset);
        try t.expectEqual(types.Address.from(0x401020), fde.addr_range.low);
        try t.expectEqual(types.Address.from(0x401070), fde.addr_range.high);
    }

    {
        const fde = cie.fdes[3];
        try t.expectEqual(0x6c, fde.cie_offset);
        try t.expectEqual(types.Address.from(0x401156), fde.addr_range.low);
        try t.expectEqual(types.Address.from(0x4011ab), fde.addr_range.high);
    }
}
