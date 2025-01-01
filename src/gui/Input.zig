const std = @import("std");
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const Mutex = Thread.Mutex;
const mem = std.mem;
const Thread = std.Thread;

const flags = @import("../flags.zig");
const logging = @import("../logging.zig");
const safe = @import("../safe.zig");
const trace = @import("../trace.zig");
const zui = @import("zui.zig");

const glfw = @import("zglfw");
const cimgui = @import("cimgui");
const imgui = cimgui.c;

const log = logging.Logger.init(logging.Region.GUI);

pub var previous_mouse_pos: [2]f32 = [2]f32{ 0, 0 };
pub var mouse_was_moved: bool = false;

pub var down_keys: ArrayList(KeyPress) = undefined;
pub var keys_mu: Mutex = Mutex{};

const KeyPress = struct {
    first: bool = true,
    first_handled: bool = false,

    action: glfw.Action,
    key: glfw.Key,
    mods: glfw.Mods,
};

pub fn init(alloc: Allocator) void {
    const z = trace.zone(@src());
    defer z.end();

    down_keys = ArrayList(KeyPress).init(alloc);
}

pub fn deinit() void {
    const z = trace.zone(@src());
    defer z.end();

    down_keys.deinit();
}

pub fn keyPressed(key: glfw.Key) bool {
    return keyWasPressed(key, false);
}

pub fn keyPressedWithCtrl(key: glfw.Key) bool {
    return keyWasPressed(key, true);
}

fn keyWasPressed(key: glfw.Key, ctrl: bool) bool {
    const z = trace.zone(@src());
    defer z.end();

    keys_mu.lock();
    defer keys_mu.unlock();

    for (down_keys.items) |*k| {
        if (key != k.key or k.mods.control != ctrl) continue;

        if (k.first and !k.first_handled) {
            // this is the very first time a key was down
            k.first_handled = true;
            return true;
        } else if (k.first) {
            // this is after the very first time a key was
            // down, and before it starts repeating
            return false;
        } else {
            // the key is held down and repeating
            return true;
        }
    }

    return false;
}

pub fn keyCallback(
    win: *glfw.Window,
    key: glfw.Key,
    scancode: i32,
    action: glfw.Action,
    mods: glfw.Mods,
) callconv(.C) void {
    const z = trace.zone(@src());
    defer z.end();

    keys_mu.lock();
    defer keys_mu.unlock();

    switch (action) {
        .release => {
            for (down_keys.items, 0..) |*k, ndx| {
                if (k.key == key) {
                    _ = down_keys.swapRemove(ndx);
                }
            }
        },
        .press => {
            down_keys.append(.{
                .action = action,
                .key = key,
                .mods = mods,
            }) catch |err| {
                log.errf("unable to append to down_keys: {!}", .{err});
            };
        },
        .repeat => {
            for (down_keys.items) |*k| {
                if (k.key == key) {
                    k.first = false;
                }
            }
        },
    }

    // pass along the key callback information to imgui
    cimgui.glfwKeyCallback(
        win,
        @intFromEnum(key),
        scancode,
        @intFromEnum(action),
        @bitCast(mods),
    );
}

pub fn calculateMouseMovement() void {
    const z = trace.zone(@src());
    defer z.end();

    const pos = zui.getMousePos();
    const mouseMove = [2]f32{
        previous_mouse_pos[0] - pos.x,
        previous_mouse_pos[1] - pos.y,
    };

    mouse_was_moved = !safe.floatsEql(mouseMove[0], 0) or !safe.floatsEql(mouseMove[1], 0);
    previous_mouse_pos[0] = pos.x;
    previous_mouse_pos[1] = pos.y;
}

/// cancelPressed is an application-wide standard set of inputs that cancel an action
pub fn cancelPressed() bool {
    return keyPressed(.escape) or
        keyPressedWithCtrl(.c) or
        keyPressedWithCtrl(.d) or
        keyPressedWithCtrl(.q);
}
