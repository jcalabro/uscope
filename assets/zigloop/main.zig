const std = @import("std");

pub fn main() !void {
    const pid = std.os.linux.getpid();

    var ndx: usize = 0;
    while (true) {
        std.debug.print("zig looping (pid {d}): {d}\n", .{
            pid,
            ndx,
        });
        std.Thread.sleep(std.time.ns_per_s);
        ndx += 1;
    }
}
