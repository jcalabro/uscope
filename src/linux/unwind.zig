//! Responsible for generating stack traces in a paused program on Linux

const std = @import("std");
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const mem = std.mem;
const pow = std.math.pow;

const arch = @import("../arch.zig").arch;
const Adapter = @import("Adapter.zig");
const consts = @import("dwarf/consts.zig");
const dwarf = @import("dwarf.zig");
const frame = @import("dwarf/frame.zig");
const logging = @import("../logging.zig");
const Reader = @import("../Reader.zig");
const safe = @import("../safe.zig");
const trace = @import("../trace.zig");
const types = @import("../types.zig");

const log = logging.Logger.init(logging.Region.Linux);

const NumFrameRegisters = 17;
const ColCFARegister = NumFrameRegisters;
const ColCFAOffset = ColCFARegister + 1;

/// +2 for CFA and Offset
const FrameTableRegisterRules = [NumFrameRegisters + 2]u8;
/// +2 for CFA and Offset
const FrameTableRegisterValues = [NumFrameRegisters + 2]i128;

const FrameTable = ArrayList(FrameTableRow);

const FrameTableRow = struct {
    /// The address of the row
    location: u64,

    rules: FrameTableRegisterRules = mem.zeroes(FrameTableRegisterRules),
    values: FrameTableRegisterValues = mem.zeroes(FrameTableRegisterValues),
};

/// unwindStack should be called when the subordinate is stopped. It uses the DWARF .eh_frame
/// and/or .debug_frame information to figure out the full backtrace of the stopped subordinate.
/// Caller owns returned memory, and the passed allocator must be a scratch arena.
pub fn stack(
    adapter: *Adapter,
    scratch: Allocator,
    pid: types.PID,
    regs: *const arch.Registers,
    load_addr: types.Address,
    addr_size: types.AddressSize,
    cie: *const frame.CIE,
    depth: ?u32, // null indicates that we should unwind the entire stack
) !types.UnwindResult {
    const z = trace.zone(@src());
    defer z.end();

    var unwind = UnwindData{
        .scratch = scratch,
        .pid = pid,
        .regs = regs,
        .addr_size = addr_size,
        .load_addr = load_addr,
        .addr = regs.pc().sub(load_addr),
        .cie = cie,
    };

    var frame_addrs = ArrayList(types.Address).init(scratch);
    var cfa: ?types.Address = null;

    const max = pow(usize, 2, 12);
    for (0..max) |frame_ndx| done: {
        defer assert(frame_ndx < max - 1);

        var found = false;
        for (cie.fdes) |*fde| {
            if (!fde.addr_range.contains(unwind.addr)) continue;

            found = true;
            unwind.fde = fde;

            try calculateFrameAddress(adapter, &unwind);
            if (cfa == null) cfa = types.Address.from(unwind.cfa);

            if (unwind.addr.int() == 0) break :done;

            // @QUESTION (jrc) why do we sometimes get duplicates?
            if (frame_addrs.items.len == 0 or frame_addrs.items[frame_addrs.items.len - 1] != unwind.addr) {
                try frame_addrs.append(unwind.addr);
            }
            unwind.addr = unwind.next_addr;

            // have we reached the end, or the maximum requested depth?
            if (unwind.addr.int() == 0 or (depth != null and frame_addrs.items.len >= depth.?)) break :done;

            break;
        }

        // no FDE found, nothing more we can do
        if (!found) break;
    }

    // copy to the final scratch region in platform-independent types
    const res = try scratch.alloc(types.Address, frame_addrs.items.len);
    @memcpy(res, frame_addrs.items);

    for (res, 0..) |addr, ndx| {
        res[ndx] = addr.add(load_addr);
    }

    return .{
        .call_stack_addrs = res,
        .frame_base_addr = cfa.?,
    };
}

const UnwindData = struct {
    scratch: Allocator,
    pid: types.PID,
    regs: *const arch.Registers,
    addr_size: types.AddressSize,

    load_addr: types.Address,

    addr: types.Address,
    next_addr: types.Address = types.Address.from(0),

    cfa: u64 = 0,
    virtual_regs: FrameTableRegisterValues = mem.zeroes(FrameTableRegisterValues),

    first_iteration: bool = true,

    cie: *const frame.CIE,
    fde: *const frame.FDE = undefined,
};

fn calculateFrameAddress(adapter: *Adapter, unwind: *UnwindData) !void {
    const z = trace.zone(@src());
    defer z.end();

    defer unwind.first_iteration = false;

    var table = FrameTable.init(unwind.scratch);
    const initial_row = try table.addOne();
    initial_row.* = .{ .location = unwind.fde.addr_range.low.int() };

    for (0..NumFrameRegisters) |ndx| {
        initial_row.values[ndx] = @intCast(try loadUnwindRegisterValueReal(unwind, ndx));
    }

    // upon first call, load the initial instructions in the CIE, starting with the default register set
    var cie_instructions_r: Reader = undefined;
    cie_instructions_r.init(unwind.cie.initial_instructions);
    try parseUnwindProgram(unwind.scratch, unwind.cie, unwind.fde.addr_range.low, &cie_instructions_r, &table, null);
    const initial_instructions_result = table.items[table.items.len - 1];

    // after loading the CIE's initial instructions, load the FDE's instructions
    var fde_instructions_r: Reader = undefined;
    fde_instructions_r.init(unwind.fde.instructions);
    try parseUnwindProgram(unwind.scratch, unwind.cie, unwind.fde.addr_range.low, &fde_instructions_r, &table, initial_instructions_result);

    unwind.cfa = switch (unwind.first_iteration) {
        false => @intCast(unwind.virtual_regs[ColCFARegister]),

        true => blk: {
            // before evaluating other instructions, calculate the CFA of the real, current frame the
            // subordinate is paused at since other rules depend on it
            const row = r: {
                var row_ndx: ?usize = null;
                for (table.items, 0..) |r, ndx| {
                    if (r.location > unwind.addr.int()) break;

                    row_ndx = ndx;
                }

                if (row_ndx) |i| break :r table.items[i];

                log.err("initial row not found");
                return error.CalculateInitialCFAError;
            };

            // initialize the virtial register table using the real values
            @memcpy(&unwind.virtual_regs, &row.values);

            // calculate the CFA of the current frame at the PC the subordinate is stopped at
            const rule = try safe.enumFromInt(consts.UnwindRegisterRule, row.rules[ColCFARegister]);
            break :blk switch (rule) {
                .undef => 0,
                .reg => r: {
                    // CFA is stored in a register + offset
                    const register = unwind.virtual_regs[ColCFARegister];
                    const offset = unwind.virtual_regs[ColCFAOffset];

                    const base = try loadUnwindRegisterValueReal(unwind, register);
                    break :r dwarf.applyOffset(@intCast(base), offset);
                },

                // @TODO (jrc): implement all other values and remove this else case, though these are probably pretty rare?
                else => {
                    log.errf("invalid initial register rule: {s}", .{@tagName(rule)});
                    return error.CalculatedInitialCFAError;
                },
            };
        },
    };

    // we've reached the top of the stack frame
    if (unwind.cfa < unwind.load_addr.int()) {
        unwind.addr = types.Address.from(0);
        return;
    }

    const last_row = blk: {
        // execute all the CIE's initial instructions, then the FDE's instructions
        // until we're at the location in the FDE we care about
        var last_row_ndx: usize = 0;
        for (table.items, 0..) |row, ndx| {
            if (row.location > unwind.addr.int()) break;

            last_row_ndx = ndx;
            try applyUnwindRegisterState(adapter, unwind, &row);
        }

        break :blk table.items[last_row_ndx];
    };

    // set the CFA for the next frame up the stack
    const cfa_rule = try safe.enumFromInt(consts.UnwindRegisterRule, last_row.rules[ColCFARegister]);
    unwind.virtual_regs[ColCFARegister] = switch (cfa_rule) {
        .undef => 0,
        .reg => blk: {
            // CFA is stored in a register + offset
            const register = last_row.values[ColCFARegister];
            const offset = last_row.values[ColCFAOffset];

            var cfa = switch (unwind.first_iteration) {
                true => try loadUnwindRegisterValueReal(unwind, register),
                false => try loadUnwindRegisterValueVirtual(unwind, register),
            };

            // @QUESTION (jrc): why doesn't Go save the CFA between frames? Is that even true?
            if (cfa == 0) cfa = unwind.cfa;

            break :blk dwarf.applyOffset(@intCast(cfa), offset);
        },

        // @TODO (jrc): implement all other values and remove this else case
        else => {
            log.errf("invalid initial register rule: {s}", .{@tagName(cfa_rule)});
            return error.CalculatedInitialCFAError;
        },
    };

    unwind.next_addr = types.Address.from(@intCast(unwind.virtual_regs[unwind.cie.return_address_register]));
}

fn loadUnwindRegisterValueReal(unwind: *const UnwindData, register: i128) !i128 {
    return @intCast(try unwind.regs.fromID(@intCast(register)));
}

fn loadUnwindRegisterValueVirtual(unwind: *const UnwindData, register: i128) !i128 {
    if (register < 0 or register >= unwind.virtual_regs.len) return error.InvalidVirtualUnwindRegister;
    return unwind.virtual_regs[@intCast(register)];
}

// Applies the rules of all virtual registers
fn applyUnwindRegisterState(adapter: *Adapter, unwind: *UnwindData, row: *const FrameTableRow) !void {
    const z = trace.zone(@src());
    defer z.end();

    // copy the values from the row in question
    @memcpy(&unwind.virtual_regs, &row.values);

    for (0..NumFrameRegisters) |ndx| {
        const rule = try safe.enumFromInt(consts.UnwindRegisterRule, row.rules[ndx]);
        const val = row.values[ndx];

        switch (rule) {
            .undef => unwind.virtual_regs[ndx] = 0,
            .same => {}, // already set, nothing to do

            .offset => {
                // read one word size's worth of data at the given address in the child's memory
                const buf = try unwind.scratch.alloc(u8, unwind.addr_size.bytes());
                defer unwind.scratch.free(buf);

                const addr = types.Address.from(dwarf.applyOffset(unwind.cfa, val));
                try adapter.peekData(unwind.pid, unwind.load_addr, addr, buf);

                unwind.virtual_regs[ndx] = switch (unwind.addr_size) {
                    .four => mem.readInt(i32, @ptrCast(buf), .little),
                    .eight => mem.readInt(i64, @ptrCast(buf), .little),
                };
            },
            .val_offset => {
                unwind.virtual_regs[ndx] = dwarf.applyOffset(unwind.cfa, val);
            },

            .reg => {
                unwind.virtual_regs[ndx] = switch (unwind.first_iteration) {
                    true => try loadUnwindRegisterValueReal(unwind, val),
                    false => try loadUnwindRegisterValueVirtual(unwind, val),
                };
            },

            // @TODO (jrc): implement these rules and delete the else block once DWARF expression parsing has been implemented
            // .expr => {},
            // .val_expr => {},
            else => {
                log.errf("unsupported unwind register rule: {s}", .{@tagName(rule)});
                return error.InvalidRegisterRule;
            },
        }
    }
}

/// Creates a new table row with the Location set to the location of the
/// previous row plus the given delta. The new row has identical data to
/// the previous row.
fn newFrameTableRow(table: *FrameTable, location: *u64, delta: u64) !void {
    const previous = table.items[table.items.len - 1];

    location.* = location.* + delta;

    const next = try table.addOne();
    next.* = .{ .location = location.* + delta };
    @memcpy(&next.rules, &previous.rules);
    @memcpy(&next.values, &previous.values);
}

/// Updates the active row's register sets with the given value. A null `rule` or `value` indicates that
/// there should be no change to that field.
fn setFrameTableRegister(table: *FrameTable, register: usize, rule: ?u8, value: ?i128) !void {
    if (register >= NumFrameRegisters + 2) return error.RegisterOutOfRange;

    if (rule) |r| table.items[table.items.len - 1].rules[register] = r;
    if (value) |v| table.items[table.items.len - 1].values[register] = v;
}

fn getCFA(table: *FrameTable) i128 {
    return table.items[table.items.len - 1].values[ColCFARegister];
}

fn parseUnwindProgram(
    scratch: Allocator,
    cie: *const frame.CIE,
    base_addr: types.Address,
    instructions_r: *Reader,
    table: *FrameTable,
    initial_settings: ?FrameTableRow,
) !void {
    const rule = consts.UnwindRegisterRule;

    var location = base_addr.int();
    var implicit_stack = ArrayList(FrameTableRow).init(scratch);

    const max = pow(usize, 2, 12);
    for (0..max) |row_ndx| {
        defer assert(row_ndx < max - 1);

        const opcode = instructions_r.read(u8) catch |err| switch (err) {
            error.EndOfFile => return,
            else => |e| {
                log.errf("unable to read unwind program opcode: {!}", .{e});
                return e;
            },
        };

        // some operands and opcodes are encoded in the same first bit
        const low_6 = opcode & 0x3f; // 0011 1111
        const high_2 = try safe.enumFromInt(consts.CallFrameHighBitsInstruction, opcode & 0xc0); // 1100 0000
        switch (high_2) {
            .DW_CFA_advance_loc => {
                const delta = low_6 * cie.code_alignment_factor;
                try newFrameTableRow(table, &location, delta);
                continue;
            },

            .DW_CFA_offset => {
                const offset: i128 = @intCast(try instructions_r.readULEB128());
                const factored = offset * cie.data_alignment_factor;
                const register = low_6;
                try setFrameTableRegister(table, register, rule.offset.int(), factored);
                continue;
            },

            // reset the rule to the initial value assigned by the CIE's initial instructions
            .DW_CFA_restore => {
                if (initial_settings == null) {
                    log.err("cannot execute opcode DW_CFA_restore because the initial settings are null");
                    return error.InvalidUnwindProgram;
                }

                const register = low_6;
                const initial = initial_settings.?.rules[register];
                try setFrameTableRegister(table, register, initial, null);
                continue;
            },

            // all other opcodes have zeros in their high two bits
            .none => {},
        }

        const instruction = try safe.enumFromInt(consts.CallFrameInstruction, opcode);
        switch (instruction) {
            .DW_CFA_nop => {},

            //
            // Row creation instructions
            //

            .DW_CFA_set_loc => {
                location = cie.segment_selector_size + switch (cie.is_32_bit) {
                    true => try instructions_r.read(u32),
                    false => try instructions_r.read(u64),
                };
                try newFrameTableRow(table, &location, 0);
            },

            .DW_CFA_advance_loc1 => {
                const delta = try instructions_r.read(u8);
                try newFrameTableRow(table, &location, delta * cie.code_alignment_factor);
            },
            .DW_CFA_advance_loc2 => {
                const delta = try instructions_r.read(u16);
                try newFrameTableRow(table, &location, delta * cie.code_alignment_factor);
            },
            .DW_CFA_advance_loc4 => {
                const delta = try instructions_r.read(u32);
                try newFrameTableRow(table, &location, delta * cie.code_alignment_factor);
            },

            //
            // CFA definition instructions
            //

            .DW_CFA_def_cfa => {
                const register = try instructions_r.readULEB128();
                const offset = try instructions_r.readULEB128(); // non-factored, unsigned

                try setFrameTableRegister(table, ColCFARegister, rule.reg.int(), register);
                try setFrameTableRegister(table, ColCFAOffset, 0, offset);
            },
            .DW_CFA_def_cfa_sf => {
                const register = try instructions_r.readULEB128();
                const offset = try instructions_r.readSLEB128(); // factored, signed
                const factored = cie.data_alignment_factor * offset;

                try setFrameTableRegister(table, ColCFARegister, rule.reg.int(), register);
                try setFrameTableRegister(table, ColCFAOffset, 0, factored);
            },
            .DW_CFA_def_cfa_register => {
                const register = try instructions_r.readULEB128();
                try setFrameTableRegister(table, ColCFARegister, rule.reg.int(), register);
            },
            .DW_CFA_def_cfa_offset => {
                const offset = try instructions_r.readULEB128(); // non-factored, unsigned
                try setFrameTableRegister(table, ColCFAOffset, 0, offset);
            },
            .DW_CFA_def_cfa_offset_sf => {
                const offset = try instructions_r.readSLEB128(); // factored, signed
                const factored = cie.data_alignment_factor * offset;

                try setFrameTableRegister(table, ColCFAOffset, 0, factored);
            },
            .DW_CFA_def_cfa_expression => {
                // @TODO (jrc): push the CFA on to the stack, evaluate the expression, and set the CFA to the result
                // const expression = try readFormBlock(instructions_r);
                // ...evaluate...
                // try setFrameTableRegister(table, CFAColumn, 0, expression_result);

                return error.OpcodeNotSupported;
            },

            //
            // Register rule instructions
            //

            .DW_CFA_undefined => {
                const register = try instructions_r.readULEB128();
                try setFrameTableRegister(table, ColCFARegister, rule.undef.int(), register);
            },
            .DW_CFA_same_value => {
                const register = try instructions_r.readULEB128();
                try setFrameTableRegister(table, ColCFARegister, rule.same.int(), register);
            },
            .DW_CFA_offset_extended => {
                const register = try instructions_r.readULEB128();
                const offset: i128 = @intCast(try instructions_r.readULEB128());
                const factored = offset * cie.data_alignment_factor;
                try setFrameTableRegister(table, register, rule.offset.int(), factored);
            },
            .DW_CFA_offset_extended_sf => {
                const register = try instructions_r.readULEB128();
                const offset = try instructions_r.readSLEB128();
                const factored = offset * cie.data_alignment_factor;
                try setFrameTableRegister(table, register, rule.offset.int(), factored);
            },
            .DW_CFA_val_offset => {
                const register = try instructions_r.readULEB128();
                const offset: i128 = @intCast(try instructions_r.readULEB128());
                const factored = offset * cie.data_alignment_factor;
                try setFrameTableRegister(table, register, rule.val_offset.int(), factored);
            },
            .DW_CFA_val_offset_sf => {
                const register = try instructions_r.readULEB128();
                const offset = try instructions_r.readSLEB128();
                const factored = offset * cie.data_alignment_factor;
                try setFrameTableRegister(table, register, rule.val_offset.int(), factored);
            },
            .DW_CFA_register => {
                const register = try instructions_r.readULEB128();
                const register_val = try instructions_r.readULEB128();
                try setFrameTableRegister(table, register, rule.reg.int(), register_val);
            },
            .DW_CFA_expression => {
                // @TODO (jrc): push the CFA on to the stack, evaluate the expression, and set the register to the result
                // const register = try instructions_r.readULEB128();
                // const expression = try readFormBlock(instructions_r);
                // ...evaluate...
                // try setFrameTableRegister(table, register, rule.expr.int(), expression_result);

                return error.OpcodeNotSupported;
            },
            .DW_CFA_val_expression => {
                // @TODO (jrc): push the CFA on to the stack, evaluate the expression, and set the register to the result
                // const register = try instructions_r.readULEB128();
                // const expression = try readFormBlock(instructions_r);
                // ...evaluate...
                // try setFrameTableRegister(table, register, rule.val_expr.int(), expression_result);

                return error.OpcodeNotSupported;
            },
            .DW_CFA_restore_extended => {
                if (initial_settings == null) {
                    log.err("cannot execute opcode DW_CFA_restore_extended because the initial settings are null");
                    return error.InvalidUnwindProgram;
                }

                const register = try instructions_r.readULEB128();
                const initial = initial_settings.?.rules[register];
                try setFrameTableRegister(table, register, initial, null);
            },

            //
            // Row state instructions
            //

            .DW_CFA_remember_state => {
                try implicit_stack.append(table.items[table.items.len - 1]);
            },
            .DW_CFA_restore_state => {
                if (implicit_stack.items.len == 0) return error.NoImplicitStackRows;

                assert(implicit_stack.items.len > 0);
                const row = implicit_stack.pop().?;
                table.items[table.items.len - 1] = row;
            },

            else => {
                log.errf("unknown unwind program opcode: {s}", .{@tagName(instruction)});
                return error.InvalidUnwindProgram;
            },
        }
    }
}

/// Reads a length field, then reads that many bytes and returns them
fn readFormBlock(alloc: Allocator, r: *Reader) ![]const u8 {
    const len = r.readULEB128();
    const buf = try alloc.alloc(u8, len);
    _ = try r.readBuf(buf);
    return buf;
}
