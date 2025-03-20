//! Evaluates DWARF expressions

const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const math = std.math;
const mem = std.mem;
const posix = std.posix;
const Random = std.Random;
const t = std.testing;

const arch = @import("../arch.zig").arch;
const consts = @import("dwarf/consts.zig");
const dwarf = @import("dwarf.zig");
const Opcode = consts.ExpressionOpcode;
const logging = @import("../logging.zig");
const Reader = @import("../Reader.zig");
const strings = @import("../strings.zig");
const String = strings.String;
const safe = @import("../safe.zig");
const trace = @import("../trace.zig");
const types = @import("../types.zig");

const log = logging.Logger.init(logging.Region.Linux);

const endianness = builtin.target.cpu.arch.endian();

const PeekFunc = fn (
    pid: types.PID,
    load_addr: types.Address,
    read_at_addr: types.Address,
    data: []u8,
) EvaluationError!void;

const CalcFunc = fn (a: i64, b: i64) i64;
const CondFunc = fn (self: *Self) EvaluationError!bool;

const Self = @This();

pub const EvaluationError = error{
    InvalidLocationExpression,
    OutOfMemory,
    UnexpectedValue,
} || Reader.ReadError || posix.PtraceError;

alloc: Allocator,
pid: types.PID,
location_expression: String,
variable_size: u64,
frame_base: types.Address,
frame_base_location_expr: String,
load_addr: types.Address,
registers: *const arch.Registers,

stack: ArrayList(String) = undefined,
reader: Reader = undefined,

/// Passed allocator must be a scratch arena, and caller owns returned memory.
pub fn evaluate(state: *Self, peek_data: PeekFunc) EvaluationError!String {
    const z = trace.zone(@src());
    defer z.end();

    // Sanity check since we don't support non amd64 architectures yet. When we add support
    // for other platforms, we'll need to update these register mappings in a x-platform way.
    switch (builtin.target.cpu.arch) {
        .x86, .x86_64 => {}, // OK
        else => @compileError("invalid CPU arch (register mappings must be corrected)"),
    }

    if (state.location_expression.len == 0) {
        log.err("location expression was empty");
        return error.InvalidLocationExpression;
    }

    state.stack = ArrayList(String).init(state.alloc);

    return try state.runEvalProgram(peek_data);
}

fn runEvalProgram(state: *Self, peek_data: PeekFunc) EvaluationError!String {
    state.reader.init(state.location_expression);

    const max = math.pow(usize, 2, 20);
    for (0..max) |byte_ndx| {
        defer assert(byte_ndx < max - 1);

        if (state.reader.atEOF()) break;

        const opcode = try safe.enumFromInt(Opcode, try state.reader.read(u8));
        switch (opcode) {
            .DW_OP_nop => {}, // do nothing

            .DW_OP_addr => try state.evalAddr(peek_data),
            .DW_OP_deref => try state.evalDeref(peek_data),

            .DW_OP_const1u, .DW_OP_const1s => try state.evalConst(1),
            .DW_OP_const2u, .DW_OP_const2s => try state.evalConst(2),
            .DW_OP_const4u, .DW_OP_const4s => try state.evalConst(4),
            .DW_OP_const8u, .DW_OP_const8s => try state.evalConst(8),
            .DW_OP_constu => try state.evalULEB128(),
            .DW_OP_consts => try state.evalSLEB128(),

            .DW_OP_fbreg => try state.evalFBReg(peek_data),
            .DW_OP_call_frame_cfa => try state.evalCallFrameCFA(),

            .DW_OP_breg0 => try state.evalBReg(peek_data, 0),
            .DW_OP_breg1 => try state.evalBReg(peek_data, 1),
            .DW_OP_breg2 => try state.evalBReg(peek_data, 2),
            .DW_OP_breg3 => try state.evalBReg(peek_data, 3),
            .DW_OP_breg4 => try state.evalBReg(peek_data, 4),
            .DW_OP_breg5 => try state.evalBReg(peek_data, 5),
            .DW_OP_breg6 => try state.evalBReg(peek_data, 6),
            .DW_OP_breg7 => try state.evalBReg(peek_data, 7),
            .DW_OP_breg8 => try state.evalBReg(peek_data, 8),
            .DW_OP_breg9 => try state.evalBReg(peek_data, 9),
            .DW_OP_breg10 => try state.evalBReg(peek_data, 10),
            .DW_OP_breg11 => try state.evalBReg(peek_data, 11),
            .DW_OP_breg12 => try state.evalBReg(peek_data, 12),
            .DW_OP_breg13 => try state.evalBReg(peek_data, 13),
            .DW_OP_breg14 => try state.evalBReg(peek_data, 14),
            .DW_OP_breg15 => try state.evalBReg(peek_data, 15),
            .DW_OP_breg16 => try state.evalBReg(peek_data, 16),
            .DW_OP_breg17 => try state.evalBReg(peek_data, 17),
            .DW_OP_breg18 => try state.evalBReg(peek_data, 18),
            .DW_OP_breg19 => try state.evalBReg(peek_data, 19),
            .DW_OP_breg20 => try state.evalBReg(peek_data, 20),
            .DW_OP_breg21 => try state.evalBReg(peek_data, 21),
            .DW_OP_breg22 => try state.evalBReg(peek_data, 22),
            .DW_OP_breg23 => try state.evalBReg(peek_data, 23),
            .DW_OP_breg24 => try state.evalBReg(peek_data, 24),
            .DW_OP_breg25 => try state.evalBReg(peek_data, 25),
            .DW_OP_breg26 => try state.evalBReg(peek_data, 26),
            .DW_OP_breg27 => try state.evalBReg(peek_data, 27),
            .DW_OP_breg28 => try state.evalBReg(peek_data, 28),
            .DW_OP_breg29 => try state.evalBReg(peek_data, 29),
            .DW_OP_breg30 => try state.evalBReg(peek_data, 30),
            .DW_OP_breg31 => try state.evalBReg(peek_data, 31),

            .DW_OP_dup => {
                const len = state.stack.items.len;
                if (len == 0) {
                    log.err("cannot execute DW_OP_dup: stack is empty");
                    return error.InvalidLocationExpression;
                }

                const dupe = try safe.copySlice(
                    u8,
                    state.alloc,
                    state.stack.items[len - 1],
                );
                try state.stack.append(dupe);
            },
            .DW_OP_drop => {
                _ = state.stack.pop();
            },
            .DW_OP_over => try state.evalPick(1),
            .DW_OP_pick => try state.evalPick(try state.reader.read(u8)),

            .DW_OP_swap => {
                const len = state.stack.items.len;
                if (len < 2) {
                    log.err("cannot execute DW_OP_swap: stack requires at least two items");
                    return error.InvalidLocationExpression;
                }

                const top = state.stack.items[len - 1];
                state.stack.items[len - 1] = state.stack.items[len - 2];
                state.stack.items[len - 2] = top;
            },
            .DW_OP_rot => {
                const len = state.stack.items.len;
                if (len < 3) {
                    log.err("cannot execute DW_OP_rot: stack requires at least three items");
                    return error.InvalidLocationExpression;
                }

                // (top, second, third -> second, third, top)
                const top = state.stack.pop().?;
                const second = state.stack.pop().?;
                const third = state.stack.pop().?;

                try state.stack.append(top);
                try state.stack.append(third);
                try state.stack.append(second);
            },

            // .DW_OP_xderef => {},

            .DW_OP_abs => try state.evalInt1(calcAbs),
            .DW_OP_and => try state.evalInt2(calcBitwiseAnd),
            .DW_OP_div => try state.evalInt2(calcDiv),
            .DW_OP_minus => try state.evalInt2(calcSub),
            .DW_OP_mod => try state.evalInt2(calcMod),
            .DW_OP_mul => try state.evalInt2(calcMul),
            .DW_OP_neg => try state.evalInt1(calcNeg),
            .DW_OP_not => try state.evalInt1(calcBitwiseNot),
            .DW_OP_or => try state.evalInt2(calcBitwiseOr),
            .DW_OP_plus => try state.evalInt2(calcAdd),
            .DW_OP_plus_uconst => try state.evalUConst(),
            .DW_OP_shl => try state.evalInt2(calcShiftLeft),
            .DW_OP_shr => try state.evalShr(),
            .DW_OP_shra => try state.evalInt2(calcShiftRightArithmetic),
            .DW_OP_xor => try state.evalInt2(calcXor),

            .DW_OP_bra => try state.branch(branchFollow),
            .DW_OP_eq => try state.evalInt2(calcEqual),
            .DW_OP_ge => try state.evalInt2(calcGreaterThanOrEqual),
            .DW_OP_gt => try state.evalInt2(calcGreaterThan),
            .DW_OP_le => try state.evalInt2(calcLessThanOrEqual),
            .DW_OP_lt => try state.evalInt2(calcLessThan),
            .DW_OP_ne => try state.evalInt2(calcNotEqual),

            .DW_OP_reg0 => try state.evalRegister(0),
            .DW_OP_reg1 => try state.evalRegister(1),
            .DW_OP_reg2 => try state.evalRegister(2),
            .DW_OP_reg3 => try state.evalRegister(3),
            .DW_OP_reg4 => try state.evalRegister(4),
            .DW_OP_reg5 => try state.evalRegister(5),
            .DW_OP_reg6 => try state.evalRegister(6),
            .DW_OP_reg7 => try state.evalRegister(7),
            .DW_OP_reg8 => try state.evalRegister(8),
            .DW_OP_reg9 => try state.evalRegister(9),
            .DW_OP_reg10 => try state.evalRegister(10),
            .DW_OP_reg11 => try state.evalRegister(11),
            .DW_OP_reg12 => try state.evalRegister(12),
            .DW_OP_reg13 => try state.evalRegister(13),
            .DW_OP_reg14 => try state.evalRegister(14),
            .DW_OP_reg15 => try state.evalRegister(15),
            .DW_OP_reg16 => try state.evalRegister(16),
            .DW_OP_reg17 => try state.evalRegister(17),

            .DW_OP_lit0 => try state.evalLiteral(0),
            .DW_OP_lit1 => try state.evalLiteral(1),
            .DW_OP_lit2 => try state.evalLiteral(2),
            .DW_OP_lit3 => try state.evalLiteral(3),
            .DW_OP_lit4 => try state.evalLiteral(4),
            .DW_OP_lit5 => try state.evalLiteral(5),
            .DW_OP_lit6 => try state.evalLiteral(6),
            .DW_OP_lit7 => try state.evalLiteral(7),
            .DW_OP_lit8 => try state.evalLiteral(8),
            .DW_OP_lit9 => try state.evalLiteral(9),
            .DW_OP_lit10 => try state.evalLiteral(10),
            .DW_OP_lit11 => try state.evalLiteral(11),
            .DW_OP_lit12 => try state.evalLiteral(12),
            .DW_OP_lit13 => try state.evalLiteral(13),
            .DW_OP_lit14 => try state.evalLiteral(14),
            .DW_OP_lit15 => try state.evalLiteral(15),
            .DW_OP_lit16 => try state.evalLiteral(16),
            .DW_OP_lit17 => try state.evalLiteral(17),
            .DW_OP_lit18 => try state.evalLiteral(18),
            .DW_OP_lit19 => try state.evalLiteral(19),
            .DW_OP_lit20 => try state.evalLiteral(20),
            .DW_OP_lit21 => try state.evalLiteral(21),
            .DW_OP_lit22 => try state.evalLiteral(22),
            .DW_OP_lit23 => try state.evalLiteral(23),
            .DW_OP_lit24 => try state.evalLiteral(24),
            .DW_OP_lit25 => try state.evalLiteral(25),
            .DW_OP_lit26 => try state.evalLiteral(26),
            .DW_OP_lit27 => try state.evalLiteral(27),
            .DW_OP_lit28 => try state.evalLiteral(28),
            .DW_OP_lit29 => try state.evalLiteral(29),
            .DW_OP_lit30 => try state.evalLiteral(30),
            .DW_OP_lit31 => try state.evalLiteral(31),

            else => {
                log.errf("unsupported expression opcode: {s}", .{@tagName(opcode)});
                return error.InvalidLocationExpression;
            },
        }
    }

    if (state.stack.items.len == 0) {
        log.err("no entries on the location expression stack");
        return error.InvalidLocationExpression;
    }

    return state.stack.items[state.stack.items.len - 1];
}

test "DW_OP_dup" {
    {
        // error: stack is empty
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_dup.int()});
        defer e.deinit();

        try t.expectError(
            error.InvalidLocationExpression,
            e.expr.runEvalProgram(TestExpression.peek),
        );
    }

    {
        // OK
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_dup.int()});
        defer e.deinit();

        const expected = "123";
        try e.expr.stack.append(expected);

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        try t.expectEqual(2, e.expr.stack.items.len);
        try t.expectEqualSlices(u8, expected, val);
    }
}

test "DW_OP_drop" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_drop.int()});
    defer e.deinit();

    const expected = "123";
    try e.expr.stack.append(expected);
    try e.expr.stack.append("456");

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqual(1, e.expr.stack.items.len);
    try t.expectEqualSlices(u8, expected, val);
}

test "DW_OP_swap" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_swap.int()});
    defer e.deinit();

    try e.expr.stack.append("1");
    try e.expr.stack.append("2");
    try e.expr.stack.append("3");

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, "2", val);

    const len = e.expr.stack.items.len;
    try t.expectEqual(3, len);
    try t.expectEqualSlices(u8, "2", e.expr.stack.items[len - 1]);
    try t.expectEqualSlices(u8, "3", e.expr.stack.items[len - 2]);
}

test "DW_OP_rot" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_rot.int()});
    defer e.deinit();

    try e.expr.stack.append("1");
    try e.expr.stack.append("2");
    try e.expr.stack.append("3");

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, "2", val);

    const len = e.expr.stack.items.len;
    try t.expectEqual(3, len);
    try t.expectEqualSlices(u8, "2", e.expr.stack.items[2]);
    try t.expectEqualSlices(u8, "1", e.expr.stack.items[1]);
    try t.expectEqualSlices(u8, "3", e.expr.stack.items[0]);
}

/// Pops a single operand off the stack, transforms it to an int, then
/// runs it through the given CalcFunc, and pushes the result on the stack
fn evalInt1(state: *Self, comptime calc: CalcFunc) !void {
    if (state.stack.items.len == 0) {
        log.err("cannot run int operation: stack is empty");
        return error.InvalidLocationExpression;
    }

    const buf = state.stack.pop().?;
    return try state.evalInt(calc, buf, buf);
}

/// Pops two operands off the stack, transforms them to ints, then runs
/// them through the given CalcFunc, and pushes the result on the stack
fn evalInt2(state: *Self, comptime calc: CalcFunc) !void {
    if (state.stack.items.len < 2) {
        log.err("cannot run int operation: stack has fewer than two items");
        return error.InvalidLocationExpression;
    }

    const buf_a = state.stack.pop().?;
    const buf_b = state.stack.pop().?;
    if (buf_a.len != buf_b.len) {
        log.errf("int operand length mismatch (first: {d}, second: {d})", .{
            buf_a.len,
            buf_b.len,
        });
        return error.InvalidLocationExpression;
    }

    return try state.evalInt(calc, buf_a, buf_b);
}

fn evalInt(
    state: *Self,
    comptime calc: CalcFunc,
    buf_a: String,
    buf_b: String,
) !void {
    assert(buf_a.len == buf_b.len);

    const res = try state.alloc.alloc(u8, buf_a.len);

    switch (buf_a.len) {
        1 => {
            const a = mem.readInt(i8, @ptrCast(buf_a), endianness);
            const b = mem.readInt(i8, @ptrCast(buf_b), endianness);
            const val = calc(a, b);
            mem.writeInt(i8, @ptrCast(res), @intCast(val), endianness);
        },
        2 => {
            const a = mem.readInt(i16, @ptrCast(buf_a), endianness);
            const b = mem.readInt(i16, @ptrCast(buf_b), endianness);
            const val = calc(a, b);
            mem.writeInt(i16, @ptrCast(res), @intCast(val), endianness);
        },
        4 => {
            const a = mem.readInt(i32, @ptrCast(buf_a), endianness);
            const b = mem.readInt(i32, @ptrCast(buf_b), endianness);
            const val = calc(a, b);
            mem.writeInt(i32, @ptrCast(res), @intCast(val), endianness);
        },
        8 => {
            const a = mem.readInt(i64, @ptrCast(buf_a), endianness);
            const b = mem.readInt(i64, @ptrCast(buf_b), endianness);
            const val = calc(a, b);
            mem.writeInt(i64, @ptrCast(res), val, endianness);
        },

        else => {
            log.errf("invalid int operation byte length: {d}", .{buf_a.len});
            return error.InvalidLocationExpression;
        },
    }

    try state.stack.append(res);
}

fn evalUConst(state: *Self) !void {
    if (state.stack.items.len == 0) {
        log.err("cannot run uconst operation: stack is empty");
        return error.InvalidLocationExpression;
    }

    const a_u64 = try state.reader.readULEB128();
    const buf_b = state.stack.pop().?;
    const res = try state.alloc.alloc(u8, buf_b.len);

    @setRuntimeSafety(false);

    switch (buf_b.len) {
        1 => {
            const a: i8 = @intCast(a_u64);
            const b = mem.readInt(i8, @ptrCast(buf_b), endianness);
            mem.writeInt(i8, @ptrCast(res), a + b, endianness);
        },
        2 => {
            const a: i16 = @intCast(a_u64);
            const b = mem.readInt(i16, @ptrCast(buf_b), endianness);
            mem.writeInt(i16, @ptrCast(res), a + b, endianness);
        },
        4 => {
            const a: i32 = @intCast(a_u64);
            const b = mem.readInt(i32, @ptrCast(buf_b), endianness);
            mem.writeInt(i32, @ptrCast(res), a + b, endianness);
        },
        8 => {
            const a: i64 = @intCast(a_u64);
            const b = mem.readInt(i64, @ptrCast(buf_b), endianness);
            mem.writeInt(i64, @ptrCast(res), a + b, endianness);
        },

        else => {
            log.errf("invalid int operation byte length: {d}", .{buf_b.len});
            return error.InvalidLocationExpression;
        },
    }

    try state.stack.append(res);
}

test "evalInt" {
    {
        // one byte
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_abs.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{248});
        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(8, res);
    }

    {
        // two bytes
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_abs.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{ 0xff, 0xff });
        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i16, @ptrCast(val), endianness);
        try t.expectEqual(1, res);
    }

    {
        // four bytes
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_abs.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{ 0xfe, 0xff, 0xff, 0xff });
        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i32, @ptrCast(val), endianness);
        try t.expectEqual(2, res);
    }

    {
        // eight bytes
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_abs.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{ 0xfd, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff });
        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i64, @ptrCast(val), endianness);
        try t.expectEqual(3, res);
    }
}

test "DW_OP_plus_uconst" {
    const e = try TestExpression.init(&[_]u8{ Opcode.DW_OP_plus_uconst.int(), 3 });
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{5});
    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(8, res);
}

fn calcAbs(val: i64, _: i64) i64 {
    return @intCast(@abs(val));
}

test "DW_OP_abs" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_abs.int()});
    defer e.deinit();

    {
        // positive -> positive
        try e.expr.stack.append(&[_]u8{248});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(8, res);
    }

    {
        // negative -> positive
        try e.expr.stack.append(&[_]u8{8});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(8, res);
    }
}

fn calcNeg(val: i64, _: i64) i64 {
    return -val;
}

test "DW_OP_neg" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_neg.int()});
    defer e.deinit();

    {
        try e.expr.stack.append(&[_]u8{12});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(-12, res);
    }

    {
        try e.expr.stack.append(&[_]u8{244});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(12, res);
    }
}

fn calcMod(a: i64, b: i64) i64 {
    return @mod(a, b);
}

test "DW_OP_mod" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_mod.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{2});
    try e.expr.stack.append(&[_]u8{7});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(1, res);
}

fn calcBitwiseAnd(a: i64, b: i64) i64 {
    return a & b;
}

test "DW_OP_and" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_and.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{3});
    try e.expr.stack.append(&[_]u8{14});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(2, res);
}

fn calcBitwiseOr(a: i64, b: i64) i64 {
    return a | b;
}

test "DW_OP_or" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_or.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{3});
    try e.expr.stack.append(&[_]u8{12});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(15, res);
}

fn calcBitwiseNot(a: i64, _: i64) i64 {
    return ~a;
}

test "DW_OP_not" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_not.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{15});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(-16, res);
}

fn calcAdd(a: i64, b: i64) i64 {
    return a + b;
}

test "DW_OP_plus" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_plus.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{2});
    try e.expr.stack.append(&[_]u8{7});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(9, res);
}

fn calcSub(a: i64, b: i64) i64 {
    return a - b;
}

test "DW_OP_minus" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_minus.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{7});
    try e.expr.stack.append(&[_]u8{2});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(-5, res);
}

fn calcMul(a: i64, b: i64) i64 {
    return a * b;
}

test "DW_OP_mul" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_mul.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{2});
    try e.expr.stack.append(&[_]u8{7});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(14, res);
}

fn calcDiv(a: i64, b: i64) i64 {
    return @divFloor(a, b);
}

test "DW_OP_div" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_div.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{2});
    try e.expr.stack.append(&[_]u8{7});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(3, res);
}

fn calcShiftLeft(a: i64, b: i64) i64 {
    @setRuntimeSafety(false);

    return a << @intCast(b);
}

test "DW_OP_shl" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_shl.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{4});
    try e.expr.stack.append(&[_]u8{1});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(16, res);
}

/// evalShr performs a logical shift right. We do it by hand here rather
/// than using intOperation2 since we don't know the size of the data ahead
/// of time, and we need to preserve the high bits, rather than simply
/// allowing the transform function to operate on int64's.
fn evalShr(state: *Self) !void {
    if (state.stack.items.len == 0) {
        log.err("cannot run right shift operation: stack has fewer than two items");
        return error.InvalidLocationExpression;
    }

    const buf_a = state.stack.pop().?;
    const buf_b = state.stack.pop().?;
    const res = try state.alloc.alloc(u8, buf_b.len);

    @setRuntimeSafety(false);

    switch (buf_a.len) {
        1 => {
            const a = mem.readInt(u8, @ptrCast(buf_a), endianness);
            const b = mem.readInt(u8, @ptrCast(buf_b), endianness);
            mem.writeInt(u8, @ptrCast(res), a >> @intCast(b), endianness);
        },
        2 => {
            const a = mem.readInt(u16, @ptrCast(buf_a), endianness);
            const b = mem.readInt(u16, @ptrCast(buf_b), endianness);
            mem.writeInt(u16, @ptrCast(res), a >> @intCast(b), endianness);
        },
        4 => {
            const a = mem.readInt(u32, @ptrCast(buf_a), endianness);
            const b = mem.readInt(u32, @ptrCast(buf_b), endianness);
            mem.writeInt(u32, @ptrCast(res), a >> @intCast(b), endianness);
        },
        8 => {
            const a = mem.readInt(u64, @ptrCast(buf_a), endianness);
            const b = mem.readInt(u64, @ptrCast(buf_b), endianness);
            mem.writeInt(u64, @ptrCast(res), a >> @intCast(b), endianness);
        },

        else => {
            log.errf("invalid int operation byte length: {d}", .{buf_b.len});
            return error.InvalidLocationExpression;
        },
    }

    try state.stack.append(res);
}

test "DW_OP_shr" {
    {
        // one byte
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_shr.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{3});
        try e.expr.stack.append(&[_]u8{32});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(u8, @ptrCast(val), endianness);
        try t.expectEqual(4, res);
    }

    {
        // two bytes
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_shr.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{ 4, 0 });
        try e.expr.stack.append(&[_]u8{ 0, 0x10 });

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(u16, @ptrCast(val), endianness);
        try t.expectEqual(0x0100, res);
    }

    {
        // four bytes
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_shr.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{ 4, 0, 0, 0 });
        try e.expr.stack.append(&[_]u8{ 0, 0, 0, 0x10 });

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(u32, @ptrCast(val), endianness);
        try t.expectEqual(0x01000000, res);
    }

    {
        // eight bytes
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_shr.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{ 4, 0, 0, 0, 0, 0, 0, 0 });
        try e.expr.stack.append(&[_]u8{ 0, 0, 0, 0, 0, 0, 0, 0x10 });

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(u64, @ptrCast(val), endianness);
        try t.expectEqual(0x01000000_00000000, res);
    }
}

fn calcShiftRightArithmetic(a: i64, b: i64) i64 {
    @setRuntimeSafety(false);

    return a >> @intCast(b);
}

test "DW_OP_shra" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_shra.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{3});
    try e.expr.stack.append(&[_]u8{32});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(4, res);
}

fn calcXor(a: i64, b: i64) i64 {
    return a ^ b;
}

test "DW_OP_xor" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_xor.int()});
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{7});
    try e.expr.stack.append(&[_]u8{8});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(15, res);
}

fn evalPick(state: *Self, depth: u8) !void {
    if (depth >= state.stack.items.len) {
        log.errf("cannot eval pick: stack size of {d} is less than depth {d}", .{
            state.stack.items.len,
            depth,
        });
        return error.InvalidLocationExpression;
    }

    const entry = try safe.copySlice(u8, state.alloc, state.stack.items[depth]);
    try state.stack.append(entry);
}

test "evalPick" {
    {
        // error: invalid depth
        const e = try TestExpression.init(&[_]u8{ Opcode.DW_OP_over.int(), 0xff });
        defer e.deinit();

        try t.expectError(
            error.InvalidLocationExpression,
            e.expr.runEvalProgram(TestExpression.peek),
        );
    }

    {
        // OK, DW_OP_over
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_over.int()});
        defer e.deinit();

        try e.expr.stack.append("0");
        try e.expr.stack.append("1");
        try e.expr.stack.append("2");

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        try t.expectEqualSlices(u8, "1", val);
    }

    {
        // OK, DW_OP_pick
        const e = try TestExpression.init(&[_]u8{ Opcode.DW_OP_pick.int(), 2 });
        defer e.deinit();

        try e.expr.stack.append("0");
        try e.expr.stack.append("1");
        try e.expr.stack.append("2");

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        try t.expectEqualSlices(u8, "2", val);
    }
}

/// Reads an address from the input reader, then peeks at that address in memory
fn evalAddr(state: *Self, peek_data: PeekFunc) !void {
    const buf = try state.alloc.alloc(u8, @sizeOf(u64));
    _ = try state.reader.readBuf(buf);

    try state.readAtAddrBuf(peek_data, buf);
}

test "evalAddr" {
    const e = try TestExpression.init(undefined);
    defer e.deinit();

    const expected = [_]u8{ 1, 2, 3, 4, 5, 6, 7, 8 };

    var rng = Random.DefaultPrng.init(0x123);
    const addr = types.Address.from(Random.int(rng.random(), u64));

    const addr_buf = try e.alloc.alloc(u8, @sizeOf(u64));
    mem.writeInt(u64, @ptrCast(addr_buf), addr.int(), endianness);

    try e.setPeekData(addr, &expected);

    e.expr.location_expression = &[_]u8{
        Opcode.DW_OP_addr.int(),
        addr_buf[0],
        addr_buf[1],
        addr_buf[2],
        addr_buf[3],
        addr_buf[4],
        addr_buf[5],
        addr_buf[6],
        addr_buf[7],
    };

    const res = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, &expected, res);
}

/// Pops an address from the stack, then peeks at that address in memory
fn evalDeref(state: *Self, peek_data: PeekFunc) !void {
    const buf = state.stack.pop();
    if (buf == null) {
        log.err("unable to execute DW_OP_deref: stack is empty");
        return error.InvalidLocationExpression;
    }

    try state.readAtAddrBuf(peek_data, @constCast(buf.?));
}

fn readAtAddrBuf(state: *Self, peek_data: PeekFunc, buf: []u8) !void {
    const addr = types.Address.from(mem.readInt(u64, @ptrCast(buf), endianness));

    const data = try state.alloc.alloc(u8, state.variable_size);
    try peek_data(state.pid, state.load_addr, addr, data);
    try state.stack.append(data);
}

test "evalDeref" {
    const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_deref.int()});
    defer e.deinit();

    var rng = Random.DefaultPrng.init(0x123);
    const addr = types.Address.from(Random.int(rng.random(), u64));
    const addr_buf = try e.alloc.alloc(u8, @sizeOf(u64));
    mem.writeInt(u64, @ptrCast(addr_buf), addr.int(), endianness);
    try e.expr.stack.append(addr_buf);

    const expected = [_]u8{ 1, 2, 3, 4, 5, 6, 7, 8 };
    try e.setPeekData(addr, &expected);

    const res = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, &expected, res);
}

fn evalFBReg(state: *Self, peek_data: PeekFunc) !void {
    if (state.frame_base_location_expr.len == 0) {
        log.err("unable to calculate DW_OP_fbreg: frame base is empty");
        return error.InvalidLocationExpression;
    }

    const offset = try state.reader.readSLEB128();

    var recursive = state.*;
    recursive.location_expression = state.frame_base_location_expr;
    const res = try recursive.evaluate(peek_data);
    const addr = mem.readInt(u64, @ptrCast(res), endianness);
    const loc = dwarf.applyOffset(addr, offset);

    // @NOTE (jrc): We intentionally use zero as the load address in fbreg and breg because
    // the address we're looking up in memory already came from a register, so the value
    // in the RBP register for instance already has the load address applied
    const data = try state.alloc.alloc(u8, state.variable_size);
    try peek_data(state.pid, types.Address.from(0), types.Address.from(loc), data);
    try state.stack.append(data);
}

test "evalFBReg" {
    const offset = 5;
    const sleb = [_]u8{offset};

    const e = try TestExpression.init(&[_]u8{ Opcode.DW_OP_fbreg.int(), sleb[0] });
    defer e.deinit();

    const addr = types.Address.from(0x123);
    try e.setFrameBaseLocationExpr(.DW_OP_call_frame_cfa);
    e.expr.frame_base = addr;

    const expected = [_]u8{ 1, 2, 3, 4, 0, 0, 0, 0 };
    try e.setPeekData(addr.addInt(offset), &expected);

    const res = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, &expected, res);
}

fn evalCallFrameCFA(state: *Self) !void {
    const cfa_buf = try state.alloc.alloc(u8, @sizeOf(u64));
    mem.writeInt(@TypeOf(state.frame_base.int()), @ptrCast(cfa_buf), state.frame_base.int(), endianness);
    try state.stack.append(cfa_buf);
}

/// @NEEDSTEST
fn evalBReg(state: *Self, peek_data: PeekFunc, register: u64) !void {
    const val = try state.registers.fromID(register);
    const offset = try state.reader.readSLEB128();
    const loc = dwarf.applyOffset(val, offset);

    const data = try state.alloc.alloc(u8, state.variable_size);
    try peek_data(state.pid, types.Address.from(0), types.Address.from(loc), data);
    try state.stack.append(data);
}

fn evalBRegX(state: *Self) !void {
    const register = try state.reader.readSLEB128();
    try state.evalBReg(register);
}

fn branch(state: *Self, cond_func: CondFunc) !void {
    // 2-byte distance operand
    const distance = try state.reader.read(i16);

    if (try cond_func(state)) {
        state.reader.advanceBy(@intCast(distance));
    }
}

fn branchFollow(state: *Self) EvaluationError!bool {
    const len = state.stack.items.len;
    if (len == 0) {
        log.err("cannot run DW_OP_bra: stack is empty");
        return error.InvalidLocationExpression;
    }

    const top = state.stack.items[len - 1];

    var any_bits = false;
    for (top) |b| {
        if (b != 0) {
            any_bits = true;
            break;
        }
    }

    return any_bits;
}

test "DW_OP_bra" {
    const e = try TestExpression.init(&[_]u8{
        Opcode.DW_OP_lit8.int(),
        Opcode.DW_OP_neg.int(),
        Opcode.DW_OP_bra.int(),
        1,
        0,
        Opcode.DW_OP_neg.int(), // skipped
    });
    defer e.deinit();

    try e.expr.stack.append(&[_]u8{1});

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    const res = mem.readInt(i8, @ptrCast(val), endianness);
    try t.expectEqual(-8, res);
}

const EqualityTestCase = switch (builtin.is_test) {
    false => @compileError("EqualityTestCase may only be used in tests"),
    true => struct {
        a: u8,
        b: u8,
        res: i8,
    },
};

fn calcEqual(a: i64, b: i64) i64 {
    if (a == b) return 1;
    return 0;
}

test "DW_OP_eq" {
    const cases = [_]EqualityTestCase{
        .{ .a = 8, .b = 7, .res = 0 },
        .{ .a = 8, .b = 8, .res = 1 },
        .{ .a = 8, .b = 9, .res = 0 },
    };

    for (cases) |c| {
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_eq.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{c.a});
        try e.expr.stack.append(&[_]u8{c.b});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(c.res, res);
    }
}

fn calcNotEqual(a: i64, b: i64) i64 {
    if (a != b) return 1;
    return 0;
}

test "DW_OP_ne" {
    const cases = [_]EqualityTestCase{
        .{ .a = 8, .b = 7, .res = 1 },
        .{ .a = 8, .b = 8, .res = 0 },
        .{ .a = 8, .b = 9, .res = 1 },
    };

    for (cases) |c| {
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_ne.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{c.a});
        try e.expr.stack.append(&[_]u8{c.b});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(c.res, res);
    }
}

fn calcGreaterThanOrEqual(a: i64, b: i64) i64 {
    if (a >= b) return 1;
    return 0;
}

test "DW_OP_ge" {
    const cases = [_]EqualityTestCase{
        .{ .a = 7, .b = 8, .res = 1 },
        .{ .a = 8, .b = 8, .res = 1 },
        .{ .a = 9, .b = 8, .res = 0 },
    };

    for (cases) |c| {
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_ge.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{c.a});
        try e.expr.stack.append(&[_]u8{c.b});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(c.res, res);
    }
}

fn calcGreaterThan(a: i64, b: i64) i64 {
    if (a > b) return 1;
    return 0;
}

test "DW_OP_gt" {
    const cases = [_]EqualityTestCase{
        .{ .a = 7, .b = 8, .res = 1 },
        .{ .a = 8, .b = 8, .res = 0 },
        .{ .a = 9, .b = 8, .res = 0 },
    };

    for (cases) |c| {
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_gt.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{c.a});
        try e.expr.stack.append(&[_]u8{c.b});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(c.res, res);
    }
}

fn calcLessThanOrEqual(a: i64, b: i64) i64 {
    if (a <= b) return 1;
    return 0;
}

test "DW_OP_le" {
    const cases = [_]EqualityTestCase{
        .{ .a = 7, .b = 8, .res = 0 },
        .{ .a = 8, .b = 8, .res = 1 },
        .{ .a = 9, .b = 8, .res = 1 },
    };

    for (cases) |c| {
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_le.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{c.a});
        try e.expr.stack.append(&[_]u8{c.b});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(c.res, res);
    }
}

fn calcLessThan(a: i64, b: i64) i64 {
    if (a < b) return 1;
    return 0;
}

test "DW_OP_lt" {
    const cases = [_]EqualityTestCase{
        .{ .a = 7, .b = 8, .res = 0 },
        .{ .a = 8, .b = 8, .res = 0 },
        .{ .a = 9, .b = 8, .res = 1 },
    };

    for (cases) |c| {
        const e = try TestExpression.init(&[_]u8{Opcode.DW_OP_lt.int()});
        defer e.deinit();

        try e.expr.stack.append(&[_]u8{c.a});
        try e.expr.stack.append(&[_]u8{c.b});

        const val = try e.expr.runEvalProgram(TestExpression.peek);
        const res = mem.readInt(i8, @ptrCast(val), endianness);
        try t.expectEqual(c.res, res);
    }
}

fn evalConst(state: *Self, comptime size: u8) !void {
    const buf = try state.alloc.alloc(u8, size);
    _ = try state.reader.readBuf(buf);
    try state.stack.append(buf);
}

test "evalConst" {
    const sizes = [_]u8{ 1, 2, 4, 8 };

    inline for (sizes) |size| {
        var bytes = ArrayList(u8).init(t.allocator);
        defer bytes.deinit();

        for (0..size) |_| try bytes.append(0);
        bytes.items[bytes.items.len - 1] = 0xff;

        const opcodes = switch (size) {
            1 => [_]Opcode{ .DW_OP_const1u, .DW_OP_const1s },
            2 => [_]Opcode{ .DW_OP_const2u, .DW_OP_const2s },
            4 => [_]Opcode{ .DW_OP_const4u, .DW_OP_const4s },
            8 => [_]Opcode{ .DW_OP_const8u, .DW_OP_const8s },
            else => unreachable,
        };

        for (opcodes) |opcode| {
            var instructions = ArrayList(u8).init(t.allocator);
            defer instructions.deinit();

            try instructions.append(opcode.int());
            try instructions.appendSlice(bytes.items);

            const e = try TestExpression.init(instructions.items);
            defer e.deinit();

            const val = try e.expr.runEvalProgram(TestExpression.peek);
            try t.expectEqualSlices(u8, bytes.items, val);
            try t.expectEqual(size + 1, e.expr.reader.offset());
        }
    }
}

fn evalULEB128(state: *Self) !void {
    const n = try state.reader.readULEB128();

    const buf = try state.alloc.alloc(u8, @sizeOf(u64));
    mem.writeInt(u64, @ptrCast(buf), n, endianness);

    try state.stack.append(buf);
}

test "evalULEB128" {
    const e = try TestExpression.init(&[_]u8{ Opcode.DW_OP_constu.int(), 0xaa, 0 });
    defer e.deinit();

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, &[_]u8{ 0x2a, 0, 0, 0, 0, 0, 0, 0 }, val);
}

fn evalSLEB128(state: *Self) !void {
    const n = try state.reader.readSLEB128();

    const buf = try state.alloc.alloc(u8, @sizeOf(u64));
    mem.writeInt(i64, @ptrCast(buf), n, endianness);

    try state.stack.append(buf);
}

test "evalSLEB128" {
    const e = try TestExpression.init(&[_]u8{ Opcode.DW_OP_consts.int(), 0xaa, 0 });
    defer e.deinit();

    const val = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, &[_]u8{ 0x2a, 0, 0, 0, 0, 0, 0, 0 }, val);
}

fn evalRegister(state: *Self, register: u64) !void {
    const val = try state.registers.fromID(register);

    const buf = try state.alloc.alloc(u8, @sizeOf(@TypeOf(val)));
    mem.writeInt(u64, @ptrCast(buf), val, endianness);

    try state.stack.append(buf);
}

test "evalRegister" {
    var ndx: u8 = 0;
    while (ndx < 17) : (ndx += 1) {
        const opcode_base = Opcode.DW_OP_reg0.int();
        const opcode = opcode_base + ndx;

        const e = try TestExpression.init(&[_]u8{opcode});
        defer e.deinit();

        const res = try e.expr.runEvalProgram(TestExpression.peek);
        try t.expectEqualSlices(u8, &[_]u8{ ndx + 4, 0, 0, 0, 0, 0, 0, 0 }, res);
    }
}

fn evalLiteral(state: *Self, val: u8) !void {
    const buf = try state.alloc.alloc(u8, 1);
    buf[0] = val;
    try state.stack.append(buf);
}

test "evalLiteral" {
    var ndx: u8 = 0;
    while (ndx < 32) : (ndx += 1) {
        const opcode_base = Opcode.DW_OP_lit0.int();
        const opcode = opcode_base + ndx;

        const e = try TestExpression.init(&[_]u8{opcode});
        defer e.deinit();

        const res = try e.expr.runEvalProgram(TestExpression.peek);
        try t.expectEqualSlices(u8, &[_]u8{ndx}, res);
    }
}

test "nop opcode" {
    const e = try TestExpression.init(&[_]u8{
        Opcode.DW_OP_nop.int(),
        Opcode.DW_OP_lit1.int(),
        Opcode.DW_OP_nop.int(),
    });
    defer e.deinit();

    const res = try e.expr.runEvalProgram(TestExpression.peek);
    try t.expectEqualSlices(u8, &[_]u8{1}, res);
}

const TestExpression = switch (builtin.is_test) {
    false => @compileError("TestExpression may only be used in tests"),
    true => struct {
        var peek_values: std.AutoHashMapUnmanaged(types.Address, String) = .{};

        arena: *ArenaAllocator,
        alloc: Allocator,

        expr: Self,

        fn init(location_expression: String) !*@This() {
            peek_values = .{};

            var arena = try t.allocator.create(ArenaAllocator);
            arena.* = ArenaAllocator.init(t.allocator);
            errdefer {
                arena.deinit();
                t.allocator.destroy(arena);
            }
            const alloc = arena.allocator();

            const self = try arena.allocator().create(@This());
            self.* = .{
                .arena = arena,
                .alloc = alloc,

                .expr = .{
                    .alloc = alloc,
                    .stack = ArrayList(String).init(alloc),

                    .pid = types.PID.from(0x1),
                    .location_expression = location_expression,
                    .variable_size = 8,
                    .frame_base = types.Address.from(0),
                    .frame_base_location_expr = &.{},
                    .load_addr = types.Address.from(0x2),
                    .registers = &.{
                        .Rax = 0x4,
                        .Rdx = 0x5,
                        .Rcx = 0x6,
                        .Rbx = 0x7,
                        .Rsi = 0x8,
                        .Rdi = 0x9,
                        .Rbp = 0xa,
                        .Rsp = 0xb,
                        .R8 = 0xc,
                        .R9 = 0xd,
                        .R10 = 0xe,
                        .R11 = 0xf,
                        .R12 = 0x10,
                        .R13 = 0x11,
                        .R14 = 0x12,
                        .R15 = 0x13,
                        .Rip = 0x14,
                    },
                },
            };

            return self;
        }

        fn deinit(self: @This()) void {
            peek_values.clearAndFree(self.alloc);

            const arena = self.arena;
            self.arena.deinit();
            t.allocator.destroy(arena);
        }

        fn setFrameBaseLocationExpr(self: *@This(), op: Opcode) !void {
            const frame_base_loc = try self.alloc.alloc(u8, 1);
            frame_base_loc[0] = op.int();
            self.expr.frame_base_location_expr = frame_base_loc;
        }

        /// Creates an entry at the given address in the peek data map for later lookup. Buf must
        /// be 8 bytes because ptrace peeks data in 8 byte chunks.
        fn setPeekData(self: *@This(), addr: types.Address, buf: String) !void {
            assert(buf.len == 8);

            try peek_values.put(self.alloc, addr, buf);
        }

        /// Test stub allows the user to look up entries that were previously set in TestExpression.setPeekData
        fn peek(
            pid: types.PID,
            load_addr: types.Address,
            addr: types.Address,
            buf: []u8,
        ) !void {
            _ = load_addr;

            // we use asserts instead of `try testing.expectEqual` so the error return set is unchanged
            assert(pid.eqlInt(0x1));
            assert(buf.len == 8);

            const val = peek_values.get(addr);
            assert(val != null); // the address was not registered in the peek map
            @memcpy(buf, val.?);
        }
    },
};
