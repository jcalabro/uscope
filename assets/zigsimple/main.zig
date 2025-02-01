const std = @import("std");
const alloc = std.heap.page_allocator;

const MyStruct = struct {
    field_a: i32 = 123,
    field_b: []const u8 = "this is field_b",

    fn print(self: MyStruct, msg: []const u8) void {
        std.debug.print(
            "{s}: .{{ .field_a = {}, .field_b = \"{s}\"}}\n",
            .{ msg, self.field_a, self.field_b },
        );
    }
};

fn oneStruct() !*MyStruct {
    const s = try alloc.create(MyStruct);
    s.* = .{};
    return s;
}

fn manyStructs(num: usize) ![]MyStruct {
    const arr = try alloc.alloc(MyStruct, num);
    for (arr, 0..) |*item, ndx| {
        item.field_a = @intCast(ndx);
        item.field_b = try std.fmt.allocPrint(alloc, "str: {}", .{ndx});
    }

    return arr;
}

fn manyInts(comptime T: type, num: usize) ![]T {
    const arr = try alloc.alloc(T, num);
    for (arr, 0..) |*item, ndx| {
        if (T == f32) {
            item.* = @floatFromInt(ndx);
        } else {
            item.* = @intCast(ndx);
        }
    }

    return arr;
}

pub fn main() !void {
    const one = try oneStruct();
    one.print("one");

    defer {
        std.debug.print("defer two\n", .{});
    }
    defer std.debug.print("defer one\n", .{});

    const arr_structs = try manyStructs(4);
    for (arr_structs, 0..) |item, ndx| {
        const msg = try std.fmt.allocPrint(alloc, "arr[{}]", .{ndx});
        item.print(msg);
    }

    const arr_usizes = try manyInts(usize, 5);
    std.debug.print("arr_usizes: {any}\n", .{arr_usizes});

    const arr_floats = try manyInts(f32, 2);
    std.debug.print("arr_floats: {any}\n", .{arr_floats});

    std.debug.print("done\n", .{});
}
