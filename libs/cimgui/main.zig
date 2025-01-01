const std = @import("std");
const assert = std.debug.assert;
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const AutoHashMap = std.AutoHashMap;
const mem = std.mem;

pub const c = @import("c.zig");

pub const backend = @import("backend_glfw_opengl.zig");

var mem_allocator: ?std.mem.Allocator = null;
var mem_allocations: ?std.AutoHashMap(usize, usize) = null;
var mem_mutex: std.Thread.Mutex = .{};
const mem_alignment = 16;

var temp_buffer: ?std.ArrayList(u8) = null;

pub fn init(allocator: Allocator) void {
    if (c.igGetCurrentContext() == null) {
        mem_allocator = allocator;
        mem_allocations = AutoHashMap(usize, usize).init(allocator);
        mem_allocations.?.ensureTotalCapacity(32) catch @panic("cimgui: out of memory"); // @ROBUSTNESS (jrc)
        c.igSetAllocatorFunctions(memAlloc, memFree, null);

        _ = c.igCreateContext(null);

        temp_buffer = ArrayList(u8).init(allocator);
        temp_buffer.?.resize(3 * 1024 + 1) catch unreachable;
    }
}
pub fn deinit() void {
    if (c.igGetCurrentContext() != null) {
        temp_buffer.?.deinit();
        c.igDestroyContext(null);

        if (mem_allocations.?.count() > 0) {
            var it = mem_allocations.?.iterator();
            while (it.next()) |kv| {
                const address = kv.key_ptr.*;
                const size = kv.value_ptr.*;
                mem_allocator.?.free(@as([*]align(mem_alignment) u8, @ptrFromInt(address))[0..size]);

                // @NOTE (jrc): there are two static memory allocations that we don't specifically care
                // about getting rid of, so silence the log, but note if there are more so we don't let
                // this grow any further.
                if (mem_allocations.?.count() > 2) {
                    std.debug.print(
                        "possible memory leak or static memory usage detected: (address: 0x{x}, size: {d})\n",
                        .{ address, size },
                    );
                }
            }
            mem_allocations.?.clearAndFree();
        }

        assert(mem_allocations.?.count() == 0);
        mem_allocations.?.deinit();
        mem_allocations = null;
        mem_allocator = null;
    }
}

pub const glfwKeyCallback = ImGui_ImplGlfw_KeyCallback;
extern fn ImGui_ImplGlfw_KeyCallback(
    window: *const anyopaque, // zglfw.Window
    key: i32,
    scancode: i32,
    action: i32,
    mods: i32,
) void;

fn memAlloc(size: usize, _: ?*anyopaque) callconv(.C) ?*anyopaque {
    mem_mutex.lock();
    defer mem_mutex.unlock();

    const buf = mem_allocator.?.alignedAlloc(
        u8,
        mem_alignment,
        size,
    ) catch @panic("cimgui: out of memory"); // @ROBUSTNESS (jrc)

    mem_allocations.?.put(@intFromPtr(buf.ptr), size) catch @panic("cimgui: out of memory"); // @ROBUSTNESS (jrc)

    return buf.ptr;
}

fn memFree(maybe_ptr: ?*anyopaque, _: ?*anyopaque) callconv(.C) void {
    if (maybe_ptr) |ptr| {
        mem_mutex.lock();
        defer mem_mutex.unlock();

        if (mem_allocations != null) {
            const size = mem_allocations.?.fetchRemove(@intFromPtr(ptr)).?.value;
            const buf = @as([*]align(mem_alignment) u8, @ptrCast(@alignCast(ptr)))[0..size];
            mem_allocator.?.free(buf);
        }
    }
}
