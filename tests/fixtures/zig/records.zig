const std = @import("std");

const Inner = struct {
    signed_value: i32,
    unsigned_value: u32,
};

const Outer = struct {
    inner: Inner,
    values: [2]i32,
};

const Packed = packed struct {
    first: u3,
    second: u5,
};

var global_record = Outer{
    .inner = .{ .signed_value = -7, .unsigned_value = 9 },
    .values = .{ 20, 22 },
};

noinline fn inspectRecords(record: *const Outer, records: *const [2]Outer, slice: []const Outer, packed_record: *const Packed) bool {
    std.mem.doNotOptimizeAway(record);
    std.mem.doNotOptimizeAway(records);
    std.mem.doNotOptimizeAway(slice);
    std.mem.doNotOptimizeAway(packed_record);
    return record.inner.signed_value == -7 and record.inner.unsigned_value == 9 and
        records[1].values[1] == 44 and slice[0].values[0] == 20 and
        packed_record.first == 5 and packed_record.second == 17;
}

pub fn main() u8 {
    const records = [2]Outer{
        global_record,
        .{ .inner = .{ .signed_value = 5, .unsigned_value = 6 }, .values = .{ 43, 44 } },
    };
    const packed_record = Packed{ .first = 5, .second = 17 };
    return @intFromBool(!inspectRecords(&global_record, &records, &records, &packed_record));
}
