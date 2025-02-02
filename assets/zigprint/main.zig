const std = @import("std");
const print = std.debug.print;

const MyStruct = struct {
    field_a: i32 = 123,
    field_b: []const u8 = "this is field_b",

    fn dontOptimizeMe(_: *@This()) void {}
};

const PackedStruct = packed struct {
    first: bool = false,
    second: bool = true,
    third: u6 = 6,

    fn dontOptimizeMe(_: *@This()) void {}
};

const ExternStruct = extern struct {
    first: bool = false,
    second: bool = true,
    third: u8 = 6,

    fn dontOptimizeMe(_: *@This()) void {}
};

const MyEnum = enum(i8) {
    negative = -1,
    zero,
    first,
    second,
    final = 100,

    fn dontOptimizeMe(_: *@This()) void {}
};

pub fn main() !void {
    const a: i2 = 1;
    const b: u2 = 2;
    const c: i3 = 3;
    const d: u3 = 4;
    const e: i4 = 5;
    const f: u4 = 6;
    const g: i5 = 7;
    const h: u5 = 8;
    const i: i6 = 9;
    const j: u6 = 10;
    const k: i7 = 11;
    const l: u7 = 12;
    const m: i8 = 13;
    const n: u8 = 14;

    const o: i8 = 15;
    const p: u8 = 16;
    const q: i16 = 17;
    const r: u16 = 18;
    const s: i32 = 19;
    const t: u32 = 20;
    const u: i64 = 21;
    const v: u64 = 22;
    const w: i128 = 23;
    const x: u128 = 24;
    const y: isize = 25;
    const z: usize = 26;

    const aa: c_short = 27;
    const ab: c_ushort = 28;
    const ac: c_int = 29;
    const ad: c_uint = 30;
    const ae: c_long = 31;
    const af: c_ulong = 32;
    const ag: c_longlong = 32;
    const ah: c_ulonglong = 34;

    const ai: f32 = 35.555;
    const aj: f64 = 36.666;
    const ak: f128 = 37.777;

    const al: bool = false;
    const am: bool = true;

    const an = "hello, world!";
    const ao = try std.heap.page_allocator.alloc(u8, 4);
    @memcpy(ao, "abcd");

    var ap = MyStruct{};
    ap.dontOptimizeMe();

    var aq = PackedStruct{};
    aq.dontOptimizeMe();

    var ar = ExternStruct{};
    ar.dontOptimizeMe();

    var as = [_]u32{ '1', '2', '3', '4', '5' };
    const at = try std.heap.page_allocator.alloc(u32, as.len);
    @memcpy(at, &as);

    const au = MyEnum.negative;
    const av = MyEnum.second;
    const aw = MyEnum.final;

    const opaque_ptr: *anyopaque = @ptrFromInt(0x123);

    print("{}\n", .{a});
    print("{}\n", .{b});
    print("{}\n", .{c}); // sim:zigprint stops here
    print("{}\n", .{d});
    print("{}\n", .{e});
    print("{}\n", .{f});
    print("{}\n", .{g});
    print("{}\n", .{h});
    print("{}\n", .{i});
    print("{}\n", .{j});
    print("{}\n", .{k});
    print("{}\n", .{l});
    print("{}\n", .{m});
    print("{}\n", .{n});

    print("{}\n", .{o});
    print("{}\n", .{p});
    print("{}\n", .{q});
    print("{}\n", .{r});
    print("{}\n", .{s});
    print("{}\n", .{t});
    print("{}\n", .{u});
    print("{}\n", .{v});
    print("{}\n", .{w});
    print("{}\n", .{x});
    print("{}\n", .{y});
    print("{}\n", .{z});

    print("{}\n", .{aa});
    print("{}\n", .{ab});
    print("{}\n", .{ac});
    print("{}\n", .{ad});
    print("{}\n", .{ae});
    print("{}\n", .{af});
    print("{}\n", .{ag});
    print("{}\n", .{ah});

    print("{}\n", .{ai});
    print("{}\n", .{aj});
    print("{}\n", .{ak});

    print("{}\n", .{al});
    print("{}\n", .{am});

    print("{s}\n", .{an});
    print("{s}\n", .{ao});

    print("{}\n", .{ap});
    print("{}\n", .{aq});
    print("{}\n", .{ar});

    print("{d}\n", .{as});
    print("{d}\n", .{at});

    print("{s}\n", .{@tagName(au)});
    print("{s}\n", .{@tagName(av)});
    print("{s}\n", .{@tagName(aw)});

    print("{any}\n", .{opaque_ptr});
}
