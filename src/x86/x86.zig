const builtin = @import("builtin");

const safe = @import("../safe.zig");
const types = @import("../types.zig");

pub const InterruptInstruction = 0xcc;

pub const Registers = extern struct {
    R15: u64 = 0,
    R14: u64 = 0,
    R13: u64 = 0,
    R12: u64 = 0,
    Rbp: u64 = 0,
    Rbx: u64 = 0,
    R11: u64 = 0,
    R10: u64 = 0,
    R9: u64 = 0,
    R8: u64 = 0,
    Rax: u64 = 0,
    Rcx: u64 = 0,
    Rdx: u64 = 0,
    Rsi: u64 = 0,
    Rdi: u64 = 0,
    Orig_rax: u64 = 0,
    Rip: u64 = 0,
    Cs: u64 = 0,
    Eflags: u64 = 0,
    Rsp: u64 = 0,
    Ss: u64 = 0,
    Fs_base: u64 = 0,
    Gs_base: u64 = 0,
    Ds: u64 = 0,
    Es: u64 = 0,
    Fs: u64 = 0,
    Gs: u64 = 0,

    pub fn pc(self: Registers) types.Address {
        return types.Address.from(self.Rip);
    }

    pub fn setPC(self: *Registers, val: types.Address) void {
        self.Rip = val.int();
    }

    pub fn bp(self: Registers) types.Address {
        return types.Address.from(self.Rbp);
    }

    pub fn sp(self: Registers) types.Address {
        return types.Address.from(self.Rsp);
    }

    /// Implements platform-specific logic to retrieve the value of a registeer
    /// given an ID that is meaningful for a given target
    pub fn fromID(self: *const Registers, id: u64) error{UnexpectedValue}!u64 {
        switch (builtin.os.tag) {
            .linux => {
                const DWARFRegs = @import("../linux/dwarf/consts.zig").Registers;
                const reg = try safe.enumFromInt(DWARFRegs, id);
                return switch (reg) {
                    .rax => self.Rax,
                    .rdx => self.Rdx,
                    .rcx => self.Rcx,
                    .rbx => self.Rbx,
                    .rsi => self.Rsi,
                    .rdi => self.Rdi,
                    .rbp => self.Rbp,
                    .rsp => self.Rsp,
                    .r8 => self.R8,
                    .r9 => self.R9,
                    .r10 => self.R10,
                    .r11 => self.R11,
                    .r12 => self.R12,
                    .r13 => self.R13,
                    .r14 => self.R14,
                    .r15 => self.R15,
                    .rip => self.Rip,
                };
            },

            else => @compileError("target platform not supported"),
        }
    }
};
