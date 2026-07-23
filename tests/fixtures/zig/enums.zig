const State = enum(i8) {
    negative = -3,
    zero = 0,
    ready = 7,
};

const Tagged = union(enum) {
    unit,
    integer: u32,
};

const Raw = extern union {
    integer: i32,
    floating: f32,
};

const Failure = error{bad}!u32;

noinline fn inspectEnums(
    state: *const State,
    tagged: *const Tagged,
    raw: *const Raw,
    optional: *const ?u32,
    failure: *const Failure,
) bool {
    asm volatile ("" :: [state] "r" (state), [tagged] "r" (tagged), [raw] "r" (raw), [optional] "r" (optional), [failure] "r" (failure) : .{ .memory = true });
    return state.* == .negative and tagged.integer == 42 and raw.integer == 42 and optional.*.? == 43 and failure.* catch 0 == 44;
}

pub fn main() !void {
    const state: State = .negative;
    const tagged: Tagged = .{ .integer = 42 };
    const raw: Raw = .{ .integer = 42 };
    const optional: ?u32 = 43;
    const failure: Failure = 44;
    if (!inspectEnums(&state, &tagged, &raw, &optional, &failure)) {
        return error.UnexpectedEnumFixtureValue;
    }
}
