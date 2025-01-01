const builtin = @import("builtin");

pub const arch = switch (builtin.target.cpu.arch) {
    .x86_64 => @import("x86/x86.zig"),
    else => @compileError("unsupported arch: " ++ builtin.target),
};
