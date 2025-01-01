const std = @import("std");
const time = std.time;

pub fn main() !void {
    const pid = std.os.linux.getpid();

    var ndx: usize = 0;
    while (true) {
        std.debug.print("zig looping (pid {d}): {d}\n", .{
            pid,
            ndx,
        });
        time.sleep(time.ns_per_s);
        ndx += 1;
    }
}
