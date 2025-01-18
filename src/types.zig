const std = @import("std");
const builtin = @import("builtin");
const assert = std.debug.assert;
const mem = std.mem;
const t = std.testing;

const arch = @import("arch.zig").arch;
const file = @import("file.zig");
const flags = @import("flags.zig");
const trace = @import("trace.zig");
const safe = @import("safe.zig");
const strings = @import("strings.zig");
const String = strings.String;

/// Used when we don't know what to render
pub const Unknown = "(unknown)";

/// Creates a unique numeric type that can be used in place of primitive types
pub fn NumericType(comptime T: type) type {
    return enum(T) {
        const Self = @This();

        _,

        pub fn from(i: @typeInfo(Self).Enum.tag_type) Self {
            return @enumFromInt(i);
        }

        pub fn int(self: Self) @typeInfo(Self).Enum.tag_type {
            return @intFromEnum(self);
        }

        pub fn eql(a: Self, b: Self) bool {
            return a.int() == b.int();
        }

        pub fn eqlInt(a: Self, b: T) bool {
            return a.int() == b;
        }

        pub fn neq(a: Self, b: Self) bool {
            return a.int() != b.int();
        }

        pub fn add(a: Self, b: Self) Self {
            return Self.from(a.int() + b.int());
        }

        pub fn addInt(a: Self, b: T) Self {
            return Self.from(a.int() + b);
        }

        pub fn sub(a: Self, b: Self) Self {
            return Self.from(a.int() - b.int());
        }

        pub fn subInt(a: Self, b: T) Self {
            return Self.from(a.int() - b);
        }

        pub fn jsonStringify(self: *const Self, jw: anytype) !void {
            try jw.write(self.int());
        }

        pub fn format(self: Self, comptime fmt: String, _: std.fmt.FormatOptions, writer: anytype) !void {
            _ = try writer.print("{" ++ fmt ++ "}", .{self.int()});
        }
    };
}

/// Represents an address in memory in the subordinate process
pub const Address = NumericType(u64);

/// Indicates whether or not a compile unit is 32-bit or 64-bit. Note that regardless of
/// the architecture we're running on, we store address sizes in u64's so we are sure
/// that we're safe and stable, even if it wastes memory on smaller targets.
pub const AddressSize = enum(u4) {
    four = 4,
    eight = 8,

    pub fn bytes(self: @This()) u8 {
        return @intFromEnum(self);
    }

    pub fn bits(self: @This()) u8 {
        return self.bytes() * 8;
    }
};

/// Represents a segment of virtual memory contained between the low and
/// high addresses. A range is contained in [low,high).
pub const AddressRange = struct {
    const Self = @This();

    low: Address,
    high: Address,

    /// Returns true if addr falls in the range of [low, high)
    pub fn contains(self: Self, addr: Address) bool {
        return addr.int() >= self.low.int() and addr.int() < self.high.int();
    }

    /// For use with std.mem.sort
    pub fn sortByLowAddress(_: void, a: Self, b: Self) bool {
        return a.low.int() < b.low.int();
    }
};

/// An index in to a list of AddressRange's
pub const AddressRangeNdx = NumericType(usize);

/// Returns the containing index if addr falls in the any of the ranges of [low, high). If the
/// address is not contained in any of the ranges, returns null. The list of address ranges must
/// be sorted by low address.
pub fn sortedAddressRangesContain(ranges: []const AddressRange, addr: Address) ?AddressRangeNdx {
    const S = struct {
        fn accessor(range: AddressRange) Address {
            return range.low;
        }
    };

    return sortedAddressRangesContainByField(AddressRange, S.accessor, ranges, addr);
}

/// A sanity check intended to be used in `assert()` calls that ensures that
/// address ranges are sorted by low address
pub fn addressRangesAreSorted(ranges: []const AddressRange) bool {
    if (ranges.len == 0) return true;

    var last_low = ranges[0].low;
    var ndx: usize = 1;
    while (ndx < ranges.len) : (ndx += 1) {
        const current_low = ranges[ndx].low;
        if (last_low.int() > current_low.int()) return false;
        last_low = current_low;
    }

    return true;
}

fn sortedAddressRangesContainByField(
    comptime T: type,
    comptime accessor: fn (T) Address,
    ranges: []const T,
    addr: Address,
) ?AddressRangeNdx {
    const z = trace.zone(@src());
    defer z.end();

    // sanity check
    if (comptime builtin.mode == .Debug) {
        var last: ?Address = null;
        for (ranges) |r| {
            if (last) |l| assert(l.int() < accessor(r).int());
            last = accessor(r);
        }
    }

    var low_ndx: usize = 0;
    var high_ndx = ranges.len;

    // binary search
    while (low_ndx < high_ndx) {
        const mid_ndx = low_ndx + (high_ndx - low_ndx) / 2;
        const mid = ranges[mid_ndx];
        if (mid.contains(addr)) {
            return AddressRangeNdx.from(mid_ndx);
        }

        if (addr.int() < accessor(mid).int()) {
            high_ndx = mid_ndx;
        } else {
            low_ndx = mid_ndx + 1;
        }
    }

    return null;
}

test "addr range contains" {
    {
        // test a single range
        const range = AddressRange{ .low = Address.from(5), .high = Address.from(8) };
        try t.expect(!range.contains(Address.from(0)));
        try t.expect(!range.contains(Address.from(4)));
        try t.expect(range.contains(Address.from(5)));
        try t.expect(range.contains(Address.from(6)));
        try t.expect(range.contains(Address.from(7)));
        try t.expect(!range.contains(Address.from(8)));
    }

    {
        // test multiple ranges
        const ranges = &[_]AddressRange{
            .{ .low = Address.from(1), .high = Address.from(2) },
            .{ .low = Address.from(3), .high = Address.from(4) },
            .{ .low = Address.from(5), .high = Address.from(6) },
            .{ .low = Address.from(7), .high = Address.from(8) },
            .{ .low = Address.from(9), .high = Address.from(10) },
        };

        try t.expectEqual(AddressRangeNdx.from(0), sortedAddressRangesContain(ranges, Address.from(1)));
        try t.expectEqual(AddressRangeNdx.from(1), sortedAddressRangesContain(ranges, Address.from(3)));
        try t.expectEqual(AddressRangeNdx.from(2), sortedAddressRangesContain(ranges, Address.from(5)));
        try t.expectEqual(AddressRangeNdx.from(3), sortedAddressRangesContain(ranges, Address.from(7)));
        try t.expectEqual(AddressRangeNdx.from(4), sortedAddressRangesContain(ranges, Address.from(9)));

        try t.expectEqual(null, sortedAddressRangesContain(ranges, Address.from(0)));
        try t.expectEqual(null, sortedAddressRangesContain(ranges, Address.from(2)));
        try t.expectEqual(null, sortedAddressRangesContain(ranges, Address.from(4)));
        try t.expectEqual(null, sortedAddressRangesContain(ranges, Address.from(6)));
        try t.expectEqual(null, sortedAddressRangesContain(ranges, Address.from(8)));
        try t.expectEqual(null, sortedAddressRangesContain(ranges, Address.from(10)));
    }
}

/// Target contains various data related to the debug symbols of a subordinate process. All data
/// is loaded at once, then never again modified until the next time we do a full load.
pub const Target = struct {
    const Self = @This();

    const Flags = packed struct {
        /// Indicates whether or not the target is a position independent executable
        pie: bool,
    };

    /// Sets various options on this Target
    flags: Flags,

    /// The size of a pointer in the subordinate process (this need not be the same as a usize
    /// in the debugger process)
    addr_size: AddressSize,

    /// The string intern pool
    strings: *strings.Cache,

    /// A platform-specific implementation of a stack unwinder
    unwinder: switch (builtin.os.tag) {
        .linux => @import("linux/dwarf/frame.zig").CIEList,
        else => @compileError("build target not supported"),
    },

    /// The list of compile units for this compiled binary as they were read from debug
    /// information on disk
    compile_units: []const CompileUnit,

    /// Finds and returns a pointer to the CompileUnit that contains the given address. This function
    /// does not have knowledge of the subordinate's load address for PIE binaries.
    pub fn compileUnitForAddr(self: Self, addr: Address) ?CompileUnit {
        const z = trace.zone(@src());
        defer z.end();

        for (self.compile_units) |cu| {
            if (sortedAddressRangesContain(cu.ranges, addr) != null) return cu;
        }

        return null;
    }
};

/// An index in to a Target's compile_units list
pub const CompileUnitNdx = NumericType(usize);

/// An index in to a CompileUnit's function table
pub const FunctionNdx = NumericType(usize);

/// An index in to a CompileUnit's type table
pub const TypeNdx = NumericType(usize);

/// The result of loading debug information for a single CompileUnit from the binary on disk
pub const CompileUnit = struct {
    const Self = @This();

    pub const Functions = struct {
        pub const Range = struct {
            /// A range contained within this function's body
            range: AddressRange,
            /// The index of the Function in the CompileUnit.Functions.functions array
            func_ndx: FunctionNdx,

            pub fn sortByLowAddress(ctx: void, a: @This(), b: @This()) bool {
                return AddressRange.sortByLowAddress(ctx, a.range, b.range);
            }

            fn contains(self: @This(), addr: Address) bool {
                return self.range.contains(addr);
            }
        };

        /// All function declarations contained within this CompileUnit. This includes all
        /// normal functions as well as inlined functions.
        functions: []const Function,

        /// The list of all address ranges that correspond to various parts of all the functions
        /// in this compile unit. Each address range is mapped to an index in to the `functions`
        /// array. These ranges are sorted by low address and their lengths must be equal.
        ranges: []const Range,

        pub fn assertValid(self: @This()) void {
            if (comptime builtin.mode == .Debug) {
                // ensure all inline function indices are in range
                for (self.functions) |func| {
                    for (func.inlined_function_indices) |inline_ndx| {
                        assert(inline_ndx.int() < self.functions.len);
                    }
                }

                // ensure all function indices are in range
                for (self.ranges) |range| {
                    assert(range.func_ndx.int() < self.functions.len);
                }

                // ensure the address range list is sorted by low address
                assert(std.sort.isSorted(Range, self.ranges, {}, Range.sortByLowAddress));
            }
        }

        /// Attempts to find a function whose body contains `addr` in any of the function
        /// contained within this compile unit. Returns null if the address is not found
        /// in any address ranges.
        pub fn forAddress(self: @This(), addr: Address) ?Function {
            const z = trace.zone(@src());
            defer z.end();

            self.assertValid();

            const S = struct {
                fn accessor(range: Range) Address {
                    return range.range.low;
                }
            };

            if (sortedAddressRangesContainByField(Range, S.accessor, self.ranges, addr)) |range_ndx| {
                const func_ndx = self.ranges[range_ndx.int()].func_ndx;
                return self.functions[func_ndx.int()];
            }

            return null;
        }
    };

    /// The source code language of the compile unit
    language: Language,

    /// The size of a pointer in the compile unit
    address_size: AddressSize,

    /// The full list of all address ranges contained within this compile unit. These ranges
    /// are sorted by low address.
    ranges: []const AddressRange,

    /// The list of the source files that were used in this compile unit
    sources: []const SourceFile,

    /// The list of all data types declared within this compile unit
    data_types: []const DataType,

    /// The list of functions and their address ranges in this compile unit
    functions: Functions,

    /// All variables declared at any point in this compile unit
    variables: []const Variable,

    /// Returns true if there is an address range in this compile unit that contains `addr`
    pub fn containsAddress(self: Self, addr: Address) bool {
        return sortedAddressRangesContain(self.ranges, addr) != null;
    }

    /// Copies to `dst` from `src`. Caller owns returned memory.
    pub fn copyFrom(dst: *Self, alloc: mem.Allocator, src: Self) mem.Allocator.Error!void {
        const z = trace.zoneN(@src(), "copy compile unit");
        defer z.end();

        // @TODO (jrc) ArrayList -> ArrayListUnmanaged
        const ArrayList = std.ArrayList;

        const ranges = try safe.copySlice(AddressRange, alloc, src.ranges);
        errdefer alloc.free(ranges);

        var sources = try ArrayList(SourceFile).initCapacity(alloc, src.sources.len);
        errdefer {
            for (sources.items) |s| alloc.free(s.statements);
            sources.deinit();
        }
        for (src.sources) |s| sources.appendAssumeCapacity(.{
            .file_hash = s.file_hash,
            .statements = try safe.copySlice(SourceStatement, alloc, s.statements),
        });

        var data_types = ArrayList(DataType).init(alloc);
        errdefer {
            for (data_types.items) |dt| {
                switch (dt.form) {
                    .@"struct" => |s| alloc.free(s.members),
                    .@"union" => |u| alloc.free(u.members),
                    .@"enum" => |e| alloc.free(e.values),
                    else => {},
                }
            }
            data_types.deinit();
        }
        for (src.data_types) |src_dt| {
            var copy_dt = src_dt;
            switch (src_dt.form) {
                .@"struct" => |s| copy_dt.form.@"struct".members = try safe.copySlice(MemberType, alloc, s.members),
                .@"union" => |u| copy_dt.form.@"union".members = try safe.copySlice(MemberType, alloc, u.members),
                .@"enum" => |e| copy_dt.form.@"enum".values = try safe.copySlice(EnumValue, alloc, e.values),
                else => {},
            }
            try data_types.append(copy_dt);
        }
        // const data_types = try safe.copySlice(DataType, alloc, src.data_types);

        const variables = try safe.copySlice(Variable, alloc, src.variables);
        errdefer alloc.free(variables);

        var functions = try ArrayList(Function).initCapacity(alloc, src.functions.functions.len);
        errdefer {
            for (functions.items) |f| f.deinit(alloc);
            functions.deinit();
        }
        for (src.functions.functions) |f| {
            const stmts = try safe.copySlice(SourceStatement, alloc, f.statements);
            errdefer alloc.free(stmts);

            const addr_ranges = try safe.copySlice(AddressRange, alloc, f.addr_ranges);
            errdefer alloc.free(addr_ranges);

            const inlined_funcs = try safe.copySlice(FunctionNdx, alloc, f.inlined_function_indices);
            errdefer alloc.free(inlined_funcs);

            const vars = try safe.copySlice(VariableNdx, alloc, f.variables);
            errdefer alloc.free(vars);

            functions.appendAssumeCapacity(.{
                .name = f.name,
                .source_loc = f.source_loc,
                .statements = stmts,
                .addr_ranges = addr_ranges,
                .inlined_function_indices = inlined_funcs,
                .variables = vars,
                .platform_data = f.platform_data,
            });
        }

        const function_ranges = try safe.copySlice(CompileUnit.Functions.Range, alloc, src.functions.ranges);
        errdefer alloc.free(function_ranges);

        dst.* = .{
            .language = src.language,
            .address_size = src.address_size,
            .ranges = ranges,
            .sources = try sources.toOwnedSlice(),
            .data_types = try data_types.toOwnedSlice(),
            .variables = variables,
            .functions = .{
                .functions = try functions.toOwnedSlice(),
                .ranges = function_ranges,
            },
        };
    }
};

test "CompileUnit.Functions.forAddress" {
    var f0 = mem.zeroes(Function);
    f0.name = 0;
    var f1 = mem.zeroes(Function);
    f1.name = 1;
    var f2 = mem.zeroes(Function);
    f2.name = 2;

    const funcs = CompileUnit.Functions{
        .functions = &[_]Function{ f0, f1, f2 },
        .ranges = &[_]CompileUnit.Functions.Range{
            .{
                .range = .{ .low = Address.from(1), .high = Address.from(2) },
                .func_ndx = FunctionNdx.from(0),
            },
            .{
                .range = .{ .low = Address.from(3), .high = Address.from(4) },
                .func_ndx = FunctionNdx.from(2),
            },
            .{
                .range = .{ .low = Address.from(5), .high = Address.from(6) },
                .func_ndx = FunctionNdx.from(1),
            },
            .{
                .range = .{ .low = Address.from(7), .high = Address.from(8) },
                .func_ndx = FunctionNdx.from(2),
            },
            .{
                .range = .{ .low = Address.from(9), .high = Address.from(10) },
                .func_ndx = FunctionNdx.from(1),
            },
        },
    };

    const S = struct {
        fn check(self: CompileUnit.Functions, comptime name: strings.Hash, addr: Address) !void {
            const func = self.forAddress(addr);
            try t.expect(func != null);
            try t.expectEqual(name, func.?.name);
        }
    };

    try S.check(funcs, 0, Address.from(1));
    try S.check(funcs, 2, Address.from(3));
    try S.check(funcs, 1, Address.from(5));
    try S.check(funcs, 2, Address.from(7));
    try S.check(funcs, 1, Address.from(9));

    try t.expectEqual(null, funcs.forAddress(Address.from(0)));
    try t.expectEqual(null, funcs.forAddress(Address.from(2)));
    try t.expectEqual(null, funcs.forAddress(Address.from(4)));
    try t.expectEqual(null, funcs.forAddress(Address.from(6)));
    try t.expectEqual(null, funcs.forAddress(Address.from(8)));
    try t.expectEqual(null, funcs.forAddress(Address.from(10)));
}

/// Represents an index to a line of code within a source file. Note that
/// these are zero-indexed just like any normal array, so  the first line
/// of code in a file is at SourceLine zero.
pub const SourceLine = NumericType(u64);

/// Contains information pertaining to a single file of source code
pub const SourceFile = struct {
    const Self = @This();

    /// Uniquely identifies the aboslute path of this file
    file_hash: file.Hash,

    /// The collection of the source-level statments in the file
    statements: []const SourceStatement,
};

/// A line (or part of a line) of source code that has semantic meaning (i.e. not comment lines)
pub const SourceStatement = struct {
    /// The address at which we should set a breakpoint (i.e. at the end of the prologue)
    breakpoint_addr: Address,

    /// The line of source code to which this statement maps
    line: SourceLine,
};

/// Identifies a location in a file of source code
pub const SourceLocation = struct {
    const Self = @This();

    /// The hash of the absolute path of this source file
    file_hash: file.Hash,

    /// The line number this line of code occupies
    line: SourceLine,

    /// `column` is frequently unknown since not all compilers
    /// emit column info in their debug symbols
    column: ?usize = null,

    /// Checks whether two SourceLocations are referring to the same line of code
    pub fn eql(a: Self, b: Self) bool {
        if (a.file_hash != b.file_hash or a.line != b.line) return false;
        if (a.column == null or b.column == null) return true;
        return a.column.? == b.column.?;
    }
};

/// Language is the list of languages supported by the debugger. No other languages are supported.
pub const Language = enum(u8) {
    Unsupported,
    C,
    CPP,
    Zig,
    Rust,
    Go,
    Odin,
    Jai,
    Assembly,

    pub fn fromPath(fpath: String) @This() {
        const ext = std.fs.path.extension(fpath);

        if (strings.eql(ext, ".c")) return .C;
        if (strings.eql(ext, ".h")) return .C;

        if (strings.eql(ext, ".cc")) return .CPP;
        if (strings.eql(ext, ".cpp")) return .CPP;
        if (strings.eql(ext, ".hpp")) return .CPP;

        if (strings.eql(ext, ".zig")) return .Zig;

        if (strings.eql(ext, ".odin")) return .Odin;

        if (strings.eql(ext, ".rs")) return .Rust;

        if (strings.eql(ext, ".go")) return .Go;

        if (strings.eql(ext, ".jai")) return .Jai;

        return .Unsupported;
    }

    test "Language.fromPath" {
        const Case = struct { path: String, lang: Language };
        const cases = [_]Case{
            .{ .path = "", .lang = .Unsupported },
            .{ .path = "blah", .lang = .Unsupported },
            .{ .path = "/", .lang = .Unsupported },
            .{ .path = "zig.file", .lang = .Unsupported },
            .{ .path = "/lib64/libc.so", .lang = .Unsupported },

            .{ .path = "/path/to/file.c", .lang = .C },
            .{ .path = "/path/to/file.h", .lang = .C },

            .{ .path = "/path/to/file.cc", .lang = .CPP },
            .{ .path = "/path/to/file.cpp", .lang = .CPP },
            .{ .path = "/path/to/file.hpp", .lang = .CPP },

            .{ .path = "/path/to/file.zig", .lang = .Zig },
            .{ .path = "/path/to/file.odin", .lang = .Odin },
            .{ .path = "/path/to/file.rs", .lang = .Rust },
            .{ .path = "/path/to/file.go", .lang = .Go },
            .{ .path = "/path/to/file.jai", .lang = .Jai },
        };

        for (cases) |case| try t.expectEqual(case.lang, fromPath(case.path));
    }
};

/// Contains data for a function declaration and its body in the program text
pub const Function = struct {
    const Self = @This();

    const PlatformData = switch (builtin.os.tag) {
        .linux => struct {
            /// @DWARF: The instructions to run before executing
            /// an expression when looking up a variable value
            frame_base: strings.Hash,
        },
        else => @compileError("build target not supported"),
    };

    /// The hash of the name of the function
    name: strings.Hash,

    /// The location of this function in source code
    source_loc: ?SourceLocation,

    /// The list of source statements that compose the body of this function
    statements: []const SourceStatement,

    /// The list of address ranges for this function (note that this data is
    /// duplicated from CompileUnit.Ranges...is there a better way to store this?).
    /// This list may be unsorted.
    addr_ranges: []const AddressRange,

    /// The list of functions that have been inlined within this function. These
    /// indices point to the CompileUnit containing this Function.
    inlined_function_indices: []const FunctionNdx,

    /// Indices in to the variable list for the containing compile unit
    variables: []const VariableNdx,

    /// Data for this Function that is specific to one target OS
    platform_data: PlatformData,

    /// Free's all data for the given Function
    pub fn deinit(self: Self, alloc: mem.Allocator) void {
        alloc.free(self.statements);
        alloc.free(self.addr_ranges);
        alloc.free(self.inlined_function_indices);
        alloc.free(self.variables);
    }
};

/// A type declaration within a CompileUnit
pub const DataType = struct {
    /// The number of bytes required to store a variable of this type
    size_bytes: u32,

    /// The hash of the name of the data type
    name: strings.Hash,

    /// The form of the variable (primitive, array, struct, etc.)
    form: DataTypeForm,
};

/// One of the many forms a variable could take
pub const DataTypeForm = union(enum) {
    const Self = @This();

    unknown: UnknownType,
    primitive: PrimitiveType,
    pointer: PointerType,
    constant: ConstantType,
    @"struct": StructType,
    @"union": UnionType,
    @"enum": EnumType,
    array: ArrayType,
    typedef: TypedefType,
    function: FunctionType,
};

/// Represents a type which was not emitted in debug info by the compiler
pub const UnknownType = struct {};

/// Represents a primitive such as an int
pub const PrimitiveType = struct {
    /// How to interpret the raw data for this type
    encoding: PrimitiveTypeEncoding,
};

/// Represents the signedness of a basic integer type
pub const Signedness = enum(u1) {
    signed,
    unsigned,
};

/// Represents a pointer to some memory
pub const PointerType = struct {
    /// The data type to which this pointer points. For example, if
    /// we have a `*MyStruct`, the index would lead to `MyStruct`. This
    /// is null if the pointer type is opaque.
    data_type: ?TypeNdx,

    /// Returns the name of a pointer given the name of the type it points to.
    /// For instance, `u8` would return `*u8`.
    pub fn nameFromItemType(alloc: mem.Allocator, item_type: String) mem.Allocator.Error!String {
        return std.fmt.allocPrint(alloc, "*{s}", .{item_type});
    }
};

/// Represents a constant value of some other type
pub const ConstantType = struct {
    /// The data type which this constant is an instance of
    data_type: ?TypeNdx,
};

/// Represents a struct or a class
pub const StructType = struct {
    /// The members contained in the struct or class
    members: []const MemberType,
};

/// Represents a union
pub const UnionType = struct {
    /// The members contained in the struct or union
    members: []const MemberType,
};

/// Represents one member of a struct or class
pub const MemberType = struct {
    /// The hash of the name of the member
    name: strings.Hash,

    /// The offset of this member in memory from the start of the struct
    /// (may be null in the case of unions)
    offset_bytes: u32,

    /// A index to the type of this member in the compile unit
    data_type: TypeNdx,
};

/// Represents an enumeration
pub const EnumType = struct {
    /// The values contained in the enum
    values: []const EnumValue,
};

/// Represents one possible value in an enumeration
pub const EnumValue = struct {
    /// The hash of the name of the enum value
    name: strings.Hash,

    /// The value of this entry in the enum
    value: i128,
};

/// Represents an array or a slice
pub const ArrayType = struct {
    /// An index that indicates the type of each element contained in this array
    element_type: TypeNdx,

    /// Array length is sometimes unknown, and in that case, it's assumed that
    /// we are dealing with a null-terminated array
    len: ?u64,

    /// Returns the name of an array given the name of the type of each element. For
    /// instance, `u8` would return `[]u8`.
    pub fn nameFromItemType(alloc: mem.Allocator, item_type: String) mem.Allocator.Error!String {
        return std.fmt.allocPrint(alloc, "[]{s}", .{item_type});
    }
};

/// Represents an alias to another type definition
pub const TypedefType = struct {
    /// An index that indicates the type to which this typedef points. Not all
    /// typedefs have a type in this compile unit we can point at.
    data_type: ?TypeNdx,
};

/// Used to store pointers to functions
pub const FunctionType = struct {};

/// An index in to the variable list
pub const VariableNdx = NumericType(usize);

/// A variable in the suborindate program that will be displayed
/// to the user when it is in scope
pub const Variable = struct {
    const PlatformData = switch (builtin.os.tag) {
        .linux => struct {
            /// @DWARF: The expression to execute to look up a variable value
            location_expression: ?strings.Hash,
        },
        else => @compileError("build target not supported"),
    };

    /// The hash of the name of the variable
    name: strings.Hash,

    /// A pointer to the type of this Variable (there may be multiple types
    /// to follow in the chain before arriving at the base type)
    data_type: TypeNdx,

    /// Data for this Variable that is specific to one target OS
    platform_data: PlatformData,
};

/// The process ID assigned by the OS to a subprocess or thread
pub const PID = NumericType(i32);

/// Uniquely identifies a breakpoint. One Breakpoint set across N
/// threads all have the same BID.
pub const BID = NumericType(u64);

/// Breakpoint contains all the high-level "bookkeeping" style data related to breakpoints.
///
/// Thy represent _all_ breakpoints that have been requested by the user. They may or may
/// not yet set in the subordinate (i.e. the the user can create breakpoints even if the
/// subordinate has not yet been launched).
///
/// When we start the subordinate, before the process begins execution, we attempt to
/// apply all the breakpoints that should be set. When a new thread spawns in the
/// subordinate, we set all the breakpoints in that thread's PID and record each as
/// a new ThreadBreakpoint.
pub const Breakpoint = struct {
    const Self = @This();

    pub const Flags = packed struct {
        /// Whether or not this breakpoint has been enabled/disabled by the user
        active: bool = true,

        /// Whether or not this breakpoint was a user-requested breakpoint, or if
        /// it was set internally by the system itself
        internal: bool = false,
    };

    /// Bit flags that carry information about this breakpoint
    flags: Flags = .{},

    /// The unique identifier for this breakpoint
    bid: BID,

    /// The instruction address in the text segment at which this breakpoint is set
    addr: Address,

    /// the location of the breakpoint in source code, if there is a corresponding line
    /// of code for this breakpoint
    source_location: ?SourceLocation = null,

    /// The least-significant byte of data of the original instruction before an
    /// interrupt trap was set
    instruction_byte: u8 = undefined,

    /// The number of times this breakpoint has been hit since the user created it
    hit_count: u32 = 0,

    /// The address of the base of the stack. If provided, this breakpoint will only be
    /// triggered if the subordinate is paused at `addr` and the CFA matches `call_frame_addr`.
    /// The reason for this is that we may set some internal breakpoints when stepping that
    /// we only want to be hit in the case that we're still in the same call frame to avoid
    /// undesirable stepping behavior in recursive functions.
    call_frame_addr: ?Address = null,

    // This represents the maximum number of stack frames that may be present
    // on a "step out" operation. This is required because the way we step out
    // is by setting an internal breakpoints on the return address of the function,
    // but in the case of stepping out of a recursive function that hasn't yet
    // hit max depth, we will hit that internal breakpoint at the deepest level
    // of the callstack, so we intentionall ignore those breakpoint hits until the
    // correct callstack depth is reached.
    max_stack_frames: ?usize = null,

    /// For use with std.sort
    pub fn sort(_: void, a: Self, b: Self) bool {
        return a.bid.int() < b.bid.int();
    }
};

/// Contains the data that is set for each breakpoint on a per-thread basis
pub const ThreadBreakpoint = struct {
    // @TODO (jrc): we might have a lot of ThreadBreakpoints, so we should
    // remove bools-in-structs and just keep multiple collections
    pub const Flags = packed struct {
        /// Whether or not this breakpoint has been applied to the subordiante's text
        /// segment in the given thread
        is_applied: bool = true,
    };

    flags: Flags = .{},

    /// The ID of the Breakpoint from which this ThreadBreakpoint inherits
    bid: BID,

    /// The process ID of the subordinate thread on which this Breakpoint is stopped
    pid: PID,
};

/// Contains all the data that the UI needs to render the state of the world each frame.
/// This is only sent from the debugger layer to the UI layer:
///   1. When the UI requests an update
///   2. When some state changes in the debugger layer
/// By taking a full, separate copy of the data, it eliminates the chance of race conditions
/// and keeps memory management simple.
pub const StateSnapshot = struct {
    /// The list of breakpoints requested by the user
    breakpoints: []const Breakpoint,

    /// If the subordinate is stopped for any reason, this contains data describing its state
    paused: ?PauseData,
};

/// Contains all data that is relevant to the subordinate process whenever it stops. This
/// is sent from the debugger thread to the UI thread whenever the subordinate stops, even
/// if it's not stopped at a breakpoint or a line of source code we know about.
pub const PauseData = struct {
    const Self = @This();

    /// the PID of the thread on which we're stopped
    pid: PID,

    /// The values of all registers
    registers: arch.Registers,

    /// The line of source code at which we're stopped. Null if we are unable to determine
    /// the line of code.
    source_location: ?SourceLocation,

    /// A subordinate may be paused at a breakpoint, or null if we're stopped at some other point
    breakpoint: ?Breakpoint,

    /// The frame base is the value of the stack pointer just before calling in to the current function
    frame_base_addr: Address,

    /// The full list of stack frames, sorted from the top of the stack to the bottom
    stack_frames: []const StackFrame,

    /// The full contents of the hex displays the user would like to show
    hex_displays: []const HexDisplay,

    /// The list of local variable expressions and their lookup results
    locals: []const ExpressionResult,

    /// The list of watch expressions and their lookup results
    watches: []const ExpressionResult,

    /// The string intern pool. Must be free'd each time PauseData is free'd.
    strings: *strings.Cache,

    pub fn deinit(self: Self, alloc: mem.Allocator) void {
        const z = trace.zone(@src());
        defer z.end();

        alloc.free(self.stack_frames);
        alloc.free(self.hex_displays);
        alloc.free(self.locals);
        alloc.free(self.watches);

        self.strings.deinit(alloc);
    }

    /// Does a full, deep copy of all data. Caller owns returned memory.
    pub fn copy(self: Self, alloc: mem.Allocator) mem.Allocator.Error!Self {
        const z = trace.zoneN(@src(), "copy PauseData");
        defer z.end();

        const stack_frames = try safe.copySlice(StackFrame, alloc, self.stack_frames);
        errdefer alloc.free(stack_frames);

        const hex_displays = try safe.copySlice(HexDisplay, alloc, self.hex_displays);
        errdefer alloc.free(hex_displays);

        var locals_arr = std.ArrayListUnmanaged(ExpressionResult){};
        errdefer {
            for (locals_arr.items) |l| l.deinit(alloc);
            locals_arr.deinit(alloc);
        }
        for (self.locals) |l| {
            try locals_arr.append(alloc, try l.copy(alloc));
        }
        const locals = try locals_arr.toOwnedSlice(alloc);

        var watches_arr = std.ArrayListUnmanaged(ExpressionResult){};
        errdefer {
            for (watches_arr.items) |l| l.deinit(alloc);
            watches_arr.deinit(alloc);
        }
        for (self.watches) |l| {
            try watches_arr.append(alloc, try l.copy(alloc));
        }
        const watches = try watches_arr.toOwnedSlice(alloc);

        const strs = try self.strings.copy(alloc);
        errdefer strs.deinit(alloc);

        errdefer comptime unreachable;

        return Self{
            .pid = self.pid,
            .registers = self.registers,
            .source_location = self.source_location,
            .breakpoint = self.breakpoint,
            .frame_base_addr = self.frame_base_addr,
            .stack_frames = stack_frames,
            .hex_displays = hex_displays,
            .locals = locals,
            .watches = watches,
            .strings = strs,
        };
    }

    /// Looks up a string in the string intern table
    pub fn getString(self: Self, hash: strings.Hash) String {
        return self.strings.get(hash) orelse Unknown;
    }

    /// Looks up a local variable by name
    pub fn getLocalByName(self: Self, name: String) ?ExpressionResult {
        for (self.locals) |local| {
            if (self.strings.get(local.expression)) |local_name| {
                if (strings.eql(name, local_name)) return local;
            }
        }

        return null;
    }
};

/// An entry in the call stack
pub const StackFrame = struct {
    /// The address of the frame base
    address: Address,

    /// The name of the function at this address, or null if the name is unknown
    name: ?strings.Hash,
};

/// The result of performing stack unwinding
pub const UnwindResult = struct {
    /// The base address of the stack frame
    frame_base_addr: Address,

    /// These addresses are intentionally not `const` because we rewind the PC by one when
    /// we hit a breakpoint. No other locations should modify this slice.
    call_stack_addrs: []Address,
};

/// Contains everything needed to display a hex view
pub const HexDisplay = struct {
    /// The starting address of the memory region
    address: Address,

    /// The contents of the memory region to display
    contents: String,
};

/// Represents a pointer to another field in the `ExpressionRender` result
pub const ExpressionFieldNdx = NumericType(usize);

/// A view of a single expression that should be rendered
pub const ExpressionResult = struct {
    const Self = @This();

    /// The user-provided expresson, or an auto-discovered local variable name
    expression: strings.Hash,

    /// The zero'th element in the slice always indicates the first field that should be
    /// rendered. For instance, if we're rendering an array of ints, the first item is
    /// the array container, and every subsequent item is each int instance.
    fields: []const ExpressionRenderField,

    fn deinit(self: Self, alloc: mem.Allocator) void {
        for (self.fields) |f| f.deinit(alloc);
    }

    fn copy(src: Self, alloc: mem.Allocator) mem.Allocator.Error!Self {
        var fields = std.ArrayListUnmanaged(ExpressionRenderField){};
        errdefer {
            for (fields.items) |f| f.deinit(alloc);
            fields.deinit(alloc);
        }

        for (src.fields) |f| {
            var dupe = f;
            switch (f.encoding) {
                .array => |arr| dupe.encoding.array.items = try safe.copySlice(ExpressionFieldNdx, alloc, arr.items),
                .@"struct" => |s| dupe.encoding.@"struct".members = try safe.copySlice(ExpressionFieldNdx, alloc, s.members),

                // nothing to do for these tags
                .@"enum" => {},
                .primitive => {},
            }

            try fields.append(alloc, dupe);
        }

        return .{
            .expression = src.expression,
            .fields = try fields.toOwnedSlice(alloc),
        };
    }
};

/// One of the fields to be rendered as the result of an expression evaluation
pub const ExpressionRenderField = struct {
    /// The format of the data
    encoding: ExpressionFieldEncoding,

    /// Pointer to the buffer that contains the raw data to be rendered by the UI. Not all
    /// variables are rendered from this raw buffer (i.e. a slice is rendered by other means).
    data: ?strings.Hash,

    /// The name of the data type of this expression result field
    data_type_name: strings.Hash,

    /// Is set if we are rendering a pointer or array type
    address: ?Address = null,

    /// The name of the variable or member to be displayed (if we are rendering a local variable)
    name: ?strings.Hash,

    fn deinit(self: @This(), alloc: mem.Allocator) void {
        switch (self.encoding) {
            .array => |arr| alloc.free(arr.items),
            .@"struct" => |s| alloc.free(s.members),

            // nothing to do for these tags
            .@"enum" => {},
            .primitive => {},
        }
    }
};

/// Represents one of many ways to render the value of a variable
pub const ExpressionFieldEncoding = union(enum) {
    primitive: PrimitiveRenderer,
    array: ArrayRenderer,
    @"struct": StructRenderer,
    @"enum": EnumRenderer,
};

/// The full list of types that could constitute a primitive
pub const PrimitiveTypeEncoding = enum(u8) {
    boolean,
    signed,
    unsigned,
    float,
    complex,

    /// Even though some languages (i.e. C, C++, Zig) don't have "string" as a primitive,
    /// we respect that the vast majority of the time, a `char*` and `[]const u8` should
    /// be treated as some kind of string, so we use that as the common case to display
    /// it not just as binary, but also in a user-friendly format in the UI. This is why
    /// we differentiate string vs. array of bytes.
    string,
};

/// Renders a primitive type in whatever langauge we're debugging
pub const PrimitiveRenderer = struct {
    /// The format of the data
    encoding: PrimitiveTypeEncoding,
};

/// Renders arrays of other types of items
pub const ArrayRenderer = struct {
    /// Pointers to the items contained in this array in the contianing `ExpressionRenderer`
    items: []const ExpressionFieldNdx,
};

/// Renders structs and classes
pub const StructRenderer = struct {
    /// Pointers to the members of this struct in the containing `ExpressionRenderer`
    members: []const ExpressionFieldNdx,
};

/// Renders enum values, possibly a tagged union
pub const EnumRenderer = struct {
    /// A pointer to the type of data contained in this enum instance
    val: ExpressionFieldNdx,

    /// The name of the member of the enum to display for convenience (if known)
    name: ?strings.Hash,
};
