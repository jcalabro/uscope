pub var zig_boundary_input: i32 = -1;
pub var zig_boundary_sink: i32 = 0;

noinline fn markedReturns(value: i32) i32 {
    var frame: [40]i32 = undefined;
    frame[0] = value;
    if (frame[0] < 0) {
        zig_boundary_sink = 11;
        return -frame[0];
    }

    zig_boundary_sink = 22;
    return frame[0] + 1;
}

noinline fn noPrologue(value: i32) i32 {
    zig_boundary_sink = value;
    return value + 1;
}

inline fn inlineAdjust(value: i32) i32 {
    const adjusted = value + 7;
    zig_boundary_sink = adjusted;
    return adjusted * 2;
}

pub fn main() u8 {
    const input_ptr: *volatile i32 = &zig_boundary_input;
    const initial = input_ptr.*;
    const inlined = inlineAdjust(initial);
    const first = markedReturns(initial);
    zig_boundary_input = 4;
    const changed = input_ptr.*;
    const second = markedReturns(changed);
    const third = noPrologue(changed);
    return @intFromBool(!(inlined == 12 and first == 1 and second == 5 and third == 5));
}
