const std = @import("std");
const proto = @import("../debugger.zig").proto;

/// A RGBA-encoded color
pub const ColorF32 = [4]f32;

/// Burnt orange
pub const BreakpointActive: ColorF32 = .{ 0.84, 0.56, 0.19, 1.0 };

/// Somewhat light gray
pub const BreakpointInctive: ColorF32 = .{ 0.6, 0.6, 0.6, 1.0 };

/// Bright red
pub const StoppedAtLine: ColorF32 = .{ 1.0, 0.0, 0.0, 1.0 };

/// Light gray
pub const EncodingMetaText: ColorF32 = .{ 0.6, 0.6, 0.6, 1.0 };

/// Converts a log level to a color
pub fn forLevel(level: proto.MessageLevel) ColorF32 {
    return switch (level) {
        .debug => .{ 0.0, 1.0, 0.0, 1.0 },
        .info => .{ 0.05, 0.65, 0.95, 1.0 },
        .warning => .{ 0.94, 0.62, 0.05, 1.0 },
        .@"error" => .{ 0.95, 0.09, 0.05, 1.0 },
    };
}
