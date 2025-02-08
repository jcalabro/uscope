const builtin = @import("builtin");
const std = @import("std");
const assert = std.debug.assert;

const options = @import("zglfw_options");

comptime {
    _ = std.testing.refAllDeclsRecursive(@This());
}

const zglfw = @This();

fn cIntCast(value: anytype) c_int {
    const ValueType = @TypeOf(value);
    return switch (@typeInfo(ValueType)) {
        .int => @intCast(value),
        .@"enum", .enum_literal => @intFromEnum(value),
        .bool => @intFromBool(value),
        else => @compileError("Cannot cast " ++ @typeName(ValueType) ++ "to int."),
    };
}

//--------------------------------------------------------------------------------------------------
//
// Misc
//
//--------------------------------------------------------------------------------------------------
pub const Bool = enum(c_int) { _ };
pub const TRUE: Bool = @enumFromInt(1);
pub const FALSE: Bool = @enumFromInt(0);

pub const InitHint = enum(c_int) {
    joystick_hat_buttons = 0x00050001,
    angle_platform_type = 0x00050002,
    platform = 0x00050003,
    cocoa_chdir_resources = 0x00051001,
    cocoa_menubar = 0x00051002,
    x11_xcb_vulkan_surface = 0x00052001,
    wayland_libdecor = 0x00053001,
    _,

    pub const set = initHint;
};
pub fn initHint(hint: InitHint, value: anytype) Error!void {
    glfwInitHint(hint, cIntCast(value));
    try maybeError();
}
extern fn glfwInitHint(hint: InitHint, value: c_int) void;

pub fn init() Error!void {
    if (glfwInit() == TRUE) {
        return;
    }
    try maybeError();
}
extern fn glfwInit() Bool;

/// `pub fn terminate() void`
pub const terminate = glfwTerminate;
extern fn glfwTerminate() void;

/// `pub fn pollEvents() void`
pub const pollEvents = glfwPollEvents;
extern fn glfwPollEvents() void;

/// `pub fn waitEvents() void`
pub const waitEvents = glfwWaitEvents;
extern fn glfwWaitEvents() void;

/// `pub fn waitEventsTimeout(timeout: f64) void`
pub const waitEventsTimeout = glfwWaitEventsTimeout;
extern fn glfwWaitEventsTimeout(timeout: f64) void;

pub fn isVulkanSupported() bool {
    return glfwVulkanSupported() == TRUE;
}
extern fn glfwVulkanSupported() Bool;

pub fn getRequiredInstanceExtensions() Error![][*:0]const u8 {
    var count: u32 = 0;
    if (glfwGetRequiredInstanceExtensions(&count)) |extensions| {
        return @as([*][*:0]const u8, @ptrCast(extensions))[0..count];
    }
    try maybeError();
    return error.APIUnavailable;
}
extern fn glfwGetRequiredInstanceExtensions(count: *u32) ?*?[*:0]const u8;

/// `pub fn getTime() f64`
pub const getTime = glfwGetTime;
extern fn glfwGetTime() f64;

/// `pub fn setTime(time: f64) void`
pub const setTime = glfwSetTime;
extern fn glfwSetTime(time: f64) void;

pub const ErrorCode = c_int;

pub const Error = error{
    NotInitialized,
    NoCurrentContext,
    InvalidEnum,
    InvalidValue,
    OutOfMemory,
    APIUnavailable,
    VersionUnavailable,
    PlatformError,
    FormatUnavailable,
    NoWindowContext,
    CursorUnavailable,
    FeatureUnavailable,
    FeatureUnimplemented,
    PlatformUnavailable,
    Unknown,
};

fn convertError(e: ErrorCode) Error!void {
    return switch (e) {
        0 => {},
        0x00010001 => Error.NotInitialized,
        0x00010002 => Error.NoCurrentContext,
        0x00010003 => Error.InvalidEnum,
        0x00010004 => Error.InvalidValue,
        0x00010005 => Error.OutOfMemory,
        0x00010006 => Error.APIUnavailable,
        0x00010007 => Error.VersionUnavailable,
        0x00010008 => Error.PlatformError,
        0x00010009 => Error.FormatUnavailable,
        0x0001000A => Error.NoWindowContext,
        0x0001000B => Error.CursorUnavailable,
        0x0001000C => Error.FeatureUnavailable,
        0x0001000D => Error.FeatureUnimplemented,
        0x0001000E => Error.PlatformUnavailable,
        else => Error.Unknown,
    };
}

pub fn maybeError() Error!void {
    return convertError(glfwGetError(null));
}
pub fn maybeErrorString(str: *?[:0]const u8) Error!void {
    var c_str: ?[*:0]const u8 = undefined;
    convertError(glfwGetError(&c_str)) catch |err| {
        str.* = if (c_str) |s| std.mem.span(s) else null;
        return err;
    };
}

pub const getError = glfwGetError;
extern fn glfwGetError(out_desc: ?*?[*:0]const u8) ErrorCode;

pub const setErrorCallback = glfwSetErrorCallback;
extern fn glfwSetErrorCallback(?ErrorFn) ?ErrorFn;
pub const ErrorFn = *const fn (ErrorCode, desc: *?[:0]const u8) callconv(.C) void;

pub fn rawMouseMotionSupported() bool {
    return glfwRawMouseMotionSupported() == TRUE;
}
extern fn glfwRawMouseMotionSupported() Bool;

pub const makeContextCurrent = glfwMakeContextCurrent;
extern fn glfwMakeContextCurrent(window: ?*Window) void;

pub const getCurrentContext = glfwGetCurrentContext;
extern fn glfwGetCurrentContext() ?*Window;

pub const swapInterval = glfwSwapInterval;
extern fn glfwSwapInterval(interval: c_int) void;

pub const GlProc = *const anyopaque;

pub fn getProcAddress(procname: [*:0]const u8) ?GlProc {
    return glfwGetProcAddress(procname);
}
extern fn glfwGetProcAddress(procname: [*:0]const u8) ?GlProc;

//--------------------------------------------------------------------------------------------------
//
// Keyboard/Mouse
//
//--------------------------------------------------------------------------------------------------
pub const Action = enum(c_int) {
    release,
    press,
    repeat,
};

pub const MouseButton = enum(c_int) {
    left,
    right,
    middle,
    four,
    five,
    six,
    seven,
    eight,
};

pub const Key = enum(c_int) {
    unknown = -1,

    space = 32,
    apostrophe = 39,
    comma = 44,
    minus = 45,
    period = 46,
    slash = 47,
    zero = 48,
    one = 49,
    two = 50,
    three = 51,
    four = 52,
    five = 53,
    six = 54,
    seven = 55,
    eight = 56,
    nine = 57,
    semicolon = 59,
    equal = 61,
    a = 65,
    b = 66,
    c = 67,
    d = 68,
    e = 69,
    f = 70,
    g = 71,
    h = 72,
    i = 73,
    j = 74,
    k = 75,
    l = 76,
    m = 77,
    n = 78,
    o = 79,
    p = 80,
    q = 81,
    r = 82,
    s = 83,
    t = 84,
    u = 85,
    v = 86,
    w = 87,
    x = 88,
    y = 89,
    z = 90,
    left_bracket = 91,
    backslash = 92,
    right_bracket = 93,
    grave_accent = 96,
    world_1 = 161,
    world_2 = 162,

    escape = 256,
    enter = 257,
    tab = 258,
    backspace = 259,
    insert = 260,
    delete = 261,
    right = 262,
    left = 263,
    down = 264,
    up = 265,
    page_up = 266,
    page_down = 267,
    home = 268,
    end = 269,
    caps_lock = 280,
    scroll_lock = 281,
    num_lock = 282,
    print_screen = 283,
    pause = 284,
    F1 = 290,
    F2 = 291,
    F3 = 292,
    F4 = 293,
    F5 = 294,
    F6 = 295,
    F7 = 296,
    F8 = 297,
    F9 = 298,
    F10 = 299,
    F11 = 300,
    F12 = 301,
    F13 = 302,
    F14 = 303,
    F15 = 304,
    F16 = 305,
    F17 = 306,
    F18 = 307,
    F19 = 308,
    F20 = 309,
    F21 = 310,
    F22 = 311,
    F23 = 312,
    F24 = 313,
    F25 = 314,
    kp_0 = 320,
    kp_1 = 321,
    kp_2 = 322,
    kp_3 = 323,
    kp_4 = 324,
    kp_5 = 325,
    kp_6 = 326,
    kp_7 = 327,
    kp_8 = 328,
    kp_9 = 329,
    kp_decimal = 330,
    kp_divide = 331,
    kp_multiply = 332,
    kp_subtract = 333,
    kp_add = 334,
    kp_enter = 335,
    kp_equal = 336,
    left_shift = 340,
    left_control = 341,
    left_alt = 342,
    left_super = 343,
    right_shift = 344,
    right_control = 345,
    right_alt = 346,
    right_super = 347,
    menu = 348,
};

pub const Mods = packed struct(c_int) {
    shift: bool = false,
    control: bool = false,
    alt: bool = false,
    super: bool = false,
    caps_lock: bool = false,
    num_lock: bool = false,
    _padding: i26 = 0,
};
//--------------------------------------------------------------------------------------------------
//
// Cursor
//
//--------------------------------------------------------------------------------------------------
pub const Cursor = opaque {
    pub const Shape = enum(c_int) {
        arrow = 0x00036001,
        ibeam = 0x00036002,
        crosshair = 0x00036003,
        hand = 0x00036004,
        /// Previously named hresize
        resize_ew = 0x00036005,
        /// Previously named vresize
        resize_ns = 0x00036006,
        resize_nwse = 0x00036007,
        resize_nesw = 0x00036008,
        resize_all = 0x00036009,
        not_allowed = 0x0003600A,
    };

    pub const Mode = enum(c_int) {
        normal = 0x00034001,
        hidden = 0x00034002,
        disabled = 0x00034003,
        captured = 0x00034004,
    };

    pub const create = createCursor;
    pub const createStandard = createStandardCursor;
    pub const destroy = destroyCursor;
};

pub fn createCursor(width: i32, height: i32, pixels: []const u8, xhot: i32, yhot: i32) Error!*Cursor {
    assert(pixels.len == 4 * width * height);
    const image = Image{
        .width = width,
        .height = height,
        .pixels = @constCast(pixels.ptr),
    };
    if (glfwCreateCursor(&image, xhot, yhot)) |ptr| return ptr;
    try maybeError();
    unreachable;
}
extern fn glfwCreateCursor(image: *const Image, xhot: c_int, yhot: c_int) ?*Cursor;

pub fn createStandardCursor(shape: Cursor.Shape) Error!*Cursor {
    if (glfwCreateStandardCursor(shape)) |ptr| return ptr;
    try maybeError();
    unreachable;
}
extern fn glfwCreateStandardCursor(shape: Cursor.Shape) ?*Cursor;

pub const destroyCursor = glfwDestroyCursor;
extern fn glfwDestroyCursor(cursor: *Cursor) void;

//--------------------------------------------------------------------------------------------------
//
// Joystick
//
//--------------------------------------------------------------------------------------------------
pub const Joystick = enum(c_int) {
    _,

    pub const maximum_supported = 16;

    pub const ButtonAction = enum(u8) {
        release = 0,
        press = 1,
    };

    pub const isPresent = joystickPresent;
    pub const getGuid = getJoystickGUID;
    pub const getAxes = getJoystickAxes;
    pub const getButtons = getJoystickButtons;
    pub const isGamepad = joystickIsGamepad;
    pub const asGamepad = joystickAsGamepad;
};

pub fn joystickPresent(joystick: Joystick) bool {
    return glfwJoystickPresent(joystick) == TRUE;
}
extern fn glfwJoystickPresent(Joystick) Bool;

pub fn getJoystickGUID(joystick: Joystick) Error![:0]const u8 {
    if (glfwGetJoystickGUID(joystick)) |guid| {
        return std.mem.span(guid);
    }
    try maybeError();
    return "";
}
extern fn glfwGetJoystickGUID(Joystick) ?[*:0]const u8;

pub fn getJoystickAxes(joystick: Joystick) Error![]const f32 {
    var count: c_int = 0;
    if (glfwGetJoystickAxes(joystick, &count)) |state| {
        return state[0..@as(usize, @intCast(count))];
    }
    try maybeError();
    return @as([*]const f32, undefined)[0..0];
}
extern fn glfwGetJoystickAxes(Joystick, count: *c_int) ?[*]const f32;

pub fn getJoystickButtons(joystick: Joystick) Error![]const Joystick.ButtonAction {
    var count: c_int = 0;
    if (glfwGetJoystickButtons(joystick, &count)) |state| {
        return @as([]const Joystick.ButtonAction, @ptrCast(state[0..@as(usize, @intCast(count))]));
    }
    try maybeError();
    return @as([*]const Joystick.ButtonAction, undefined)[0..0];
}
extern fn glfwGetJoystickButtons(Joystick, count: *c_int) ?[*]const u8;

pub fn joystickIsGamepad(joystick: Joystick) bool {
    return glfwJoystickIsGamepad(joystick) == TRUE;
}
extern fn glfwJoystickIsGamepad(Joystick) Bool;

pub fn joystickAsGamepad(joystick: Joystick) ?Gamepad {
    return if (joystickIsGamepad(joystick)) @enumFromInt(@intFromEnum(joystick)) else null;
}

//--------------------------------------------------------------------------------------------------
//
// Gamepad
//
//--------------------------------------------------------------------------------------------------
pub const Gamepad = enum(c_int) {
    _,

    pub const Axis = enum(u8) {
        left_x = 0,
        left_y = 1,
        right_x = 2,
        right_y = 3,
        left_trigger = 4,
        right_trigger = 5,

        pub const count = std.meta.fields(@This()).len;
    };

    pub const Button = enum(u8) {
        a = 0,
        b = 1,
        x = 2,
        y = 3,
        left_bumper = 4,
        right_bumper = 5,
        back = 6,
        start = 7,
        guide = 8,
        left_thumb = 9,
        right_thumb = 10,
        dpad_up = 11,
        dpad_right = 12,
        dpad_down = 13,
        dpad_left = 14,

        pub const count = std.meta.fields(@This()).len;

        pub const cross = Button.a;
        pub const circle = Button.b;
        pub const square = Button.x;
        pub const triangle = Button.y;
    };

    pub const State = extern struct {
        comptime {
            const c = @cImport(@cInclude("GLFW/glfw3.h"));
            assert(@sizeOf(c.GLFWgamepadstate) == @sizeOf(State));
            for (std.meta.fieldNames(State)) |field_name| {
                assert(@offsetOf(c.GLFWgamepadstate, field_name) == @offsetOf(State, field_name));
            }
        }
        buttons: [Button.count]Joystick.ButtonAction = .{Joystick.ButtonAction.release} ** Button.count,
        axes: [Axis.count]f32 = .{@as(f32, 0)} ** Axis.count,
    };

    pub const getName = getGamepadName;
    pub const getState = getGamepadState;
    pub const updateMappings = updateGamepadMappings;
};

pub fn getGamepadName(gamepad: Gamepad) [:0]const u8 {
    return std.mem.span(glfwGetGamepadName(gamepad));
}
extern fn glfwGetGamepadName(Gamepad) [*:0]const u8;

pub fn getGamepadState(gamepad: Gamepad) Error!Gamepad.State {
    var state: Gamepad.State = undefined;
    if (glfwGetGamepadState(gamepad, &state) == TRUE) {
        return state;
    }
    try maybeError();
    return .{};
}
extern fn glfwGetGamepadState(Gamepad, *Gamepad.State) Bool;

pub fn updateGamepadMappings(mappings: [:0]const u8) bool {
    return glfwUpdateGamepadMappings(mappings) == TRUE;
}
extern fn glfwUpdateGamepadMappings(mappings: [*:0]const u8) Bool;

//--------------------------------------------------------------------------------------------------
//
// Monitor
//
//--------------------------------------------------------------------------------------------------
pub const Monitor = opaque {
    pub const getPrimary = zglfw.getPrimaryMonitor;
    pub const getAll = zglfw.getMonitors;
    pub const getName = zglfw.getMonitorName;
    pub const getVideoMode = zglfw.getVideoMode;
    pub const getVideoModes = zglfw.getVideoModes;

    pub fn getPos(self: *Monitor) [2]c_int {
        var xpos: c_int = 0;
        var ypos: c_int = 0;
        getMonitorPos(self, &xpos, &ypos);
        return .{ xpos, ypos };
    }
};

pub const getPrimaryMonitor = glfwGetPrimaryMonitor;
extern fn glfwGetPrimaryMonitor() ?*Monitor;

pub fn getMonitors() []*Monitor {
    var count: c_int = 0;
    if (glfwGetMonitors(&count)) |monitors| {
        return monitors[0..@as(usize, @intCast(count))];
    }
    return @as([*]*Monitor, undefined)[0..0];
}
extern fn glfwGetMonitors(count: *c_int) ?[*]*Monitor;

pub const getMonitorPos = glfwGetMonitorPos;
extern fn glfwGetMonitorPos(*Monitor, xpos: ?*c_int, ypos: ?*c_int) void;

pub fn getMonitorName(monitor: *Monitor) Error![]const u8 {
    if (glfwGetMonitorName(monitor)) |name| {
        return std.mem.span(name);
    }
    try maybeError();
    return "";
}
extern fn glfwGetMonitorName(monitor: *Monitor) ?[*:0]const u8;

pub fn getVideoMode(monitor: *Monitor) Error!*VideoMode {
    if (glfwGetVideoMode(monitor)) |video_mode| {
        return video_mode;
    }
    try maybeError();
    unreachable;
}
extern fn glfwGetVideoMode(*Monitor) ?*VideoMode;

pub fn getVideoModes(monitor: *Monitor) Error![]VideoMode {
    var count: c_int = 0;
    if (glfwGetVideoModes(monitor, &count)) |video_modes| {
        return video_modes[0..@as(usize, @intCast(count))];
    }
    try maybeError();
    return &.{};
}
extern fn glfwGetVideoModes(*Monitor, count: *c_int) ?[*]VideoMode;

pub const VideoMode = extern struct {
    comptime {
        const c = @cImport(@cInclude("GLFW/glfw3.h"));
        assert(@sizeOf(c.GLFWvidmode) == @sizeOf(VideoMode));
        for (std.meta.fieldNames(VideoMode), 0..) |field_name, i| {
            assert(@offsetOf(c.GLFWvidmode, std.meta.fieldNames(c.GLFWvidmode)[i]) ==
                @offsetOf(VideoMode, field_name));
        }
    }
    width: c_int,
    height: c_int,
    red_bits: c_int,
    green_bits: c_int,
    blue_bits: c_int,
    refresh_rate: c_int,
};
//--------------------------------------------------------------------------------------------------
//
// Image
//
//--------------------------------------------------------------------------------------------------
pub const Image = extern struct {
    comptime {
        const c = @cImport(@cInclude("GLFW/glfw3.h"));
        assert(@sizeOf(c.GLFWimage) == @sizeOf(Image));
        for (std.meta.fieldNames(Image)) |field_name| {
            assert(@offsetOf(c.GLFWimage, field_name) == @offsetOf(Image, field_name));
        }
    }
    width: c_int,
    height: c_int,
    pixels: [*]u8,
};
//--------------------------------------------------------------------------------------------------
//
// Window
//
//--------------------------------------------------------------------------------------------------
pub const Window = opaque {
    pub const Attribute = enum(c_int) {
        focused = 0x00020001,
        iconified = 0x00020002,
        resizable = 0x00020003,
        visible = 0x00020004,
        decorated = 0x00020005,
        auto_iconify = 0x00020006,
        floating = 0x00020007,
        maximized = 0x00020008,
        center_cursor = 0x00020009,
        transparent_framebuffer = 0x0002000A,
        hovered = 0x0002000B,
        focus_on_show = 0x0002000C,
        _,

        pub fn ValueType(comptime attribute: Attribute) type {
            return switch (attribute) {
                .focused,
                .iconified,
                .resizable,
                .visible,
                .decorated,
                .auto_iconify,
                .floating,
                .maximized,
                .center_cursor,
                .transparent_framebuffer,
                .hovered,
                .focus_on_show,
                => bool,
                else => c_int,
            };
        }
    };

    pub const create = zglfw.createWindow;
    pub const destroy = zglfw.destroyWindow;
    pub const getAttribute = zglfw.getWindowAttribute;
    pub const setAttribute = zglfw.setWindowAttribute;
    pub const getUserPointer = zglfw.getWindowUserPointer;
    pub const setUserPointer = zglfw.setWindowUserPointer;
    pub const setFramebufferCallback = zglfw.setFramebufferSizeCallback;
    pub const setSizeCallback = zglfw.setWindowSizeCallback;
    pub const setPosCallback = zglfw.setWindowPosCallback;
    pub const setFocusCallback = zglfw.setWindowFocusCallback;
    pub const setIconifyCallback = zglfw.setWindowIconifyCallback;
    pub const setContentScaleCallback = zglfw.setWindowContentScaleCallback;
    pub const setCloseCallback = zglfw.setWindowCloseCallback;
    pub const setKeyCallback = zglfw.setKeyCallback;
    pub const setCharCallback = zglfw.setCharCallback;
    pub const setMonitor = zglfw.setWindowMonitor;
    pub const show = zglfw.showWindow;
    pub const focus = zglfw.focusWindow;
    pub const getKey = zglfw.getKey;
    pub const getMouseButton = zglfw.getMouseButton;
    pub const setSizeLimits = zglfw.setWindowSizeLimits;
    pub const setPos = zglfw.setWindowPos;
    pub const setTitle = zglfw.setWindowTitle;
    pub const setIcon = zglfw.setWindowIcon;
    pub const shouldClose = zglfw.windowShouldClose;
    pub const setCursor = zglfw.setCursor;
    pub const setInputMode = zglfw.setInputMode;
    pub const setInputModeUntyped = zglfw.setInputModeUntyped;
    pub const swapBuffers = zglfw.swapBuffers;

    pub fn getCursorPos(self: *Window) [2]f64 {
        var xpos: f64 = 0.0;
        var ypos: f64 = 0.0;
        zglfw.getCursorPos(self, &xpos, &ypos);
        return .{ xpos, ypos };
    }

    pub fn getContentScale(self: *Window) [2]f32 {
        var xscale: f32 = 0.0;
        var yscale: f32 = 0.0;
        zglfw.getWindowContentScale(self, &xscale, &yscale);
        return .{ xscale, yscale };
    }

    pub fn getFramebufferSize(self: *Window) [2]c_int {
        var width: c_int = 0.0;
        var height: c_int = 0.0;
        zglfw.getFramebufferSize(self, &width, &height);
        return .{ width, height };
    }

    pub fn getSize(self: *Window) [2]c_int {
        var width: c_int = 0.0;
        var heght: c_int = 0.0;
        zglfw.getWindowSize(self, &width, &heght);
        return .{ width, heght };
    }

    pub fn getPos(self: *Window) [2]c_int {
        var xpos: c_int = 0.0;
        var ypos: c_int = 0.0;
        zglfw.getWindowPos(self, &xpos, &ypos);
        return .{ xpos, ypos };
    }
};

pub fn createWindow(
    width: c_int,
    height: c_int,
    title: [:0]const u8,
    monitor: ?*Monitor,
) Error!*Window {
    if (glfwCreateWindow(width, height, title, monitor, null)) |window| return window;
    try maybeError();
    unreachable;
}
extern fn glfwCreateWindow(
    width: c_int,
    height: c_int,
    title: [*:0]const u8,
    monitor: ?*Monitor,
    share: ?*Window,
) ?*Window;

pub const destroyWindow = glfwDestroyWindow;
extern fn glfwDestroyWindow(window: *Window) void;

pub fn getWindowAttribute(
    window: *Window,
    comptime attrib: Window.Attribute,
) Window.Attribute.ValueType(attrib) {
    return switch (@typeInfo(Window.Attribute.ValueType(attrib))) {
        .bool => @as(Bool, @enumFromInt(getWindowAttributeUntyped(window, attrib))) == TRUE,
        .int => getWindowAttributeUntyped(window, attrib),
        else => unreachable,
    };
}
pub const getWindowAttributeUntyped = glfwGetWindowAttrib;
extern fn glfwGetWindowAttrib(window: *Window, attrib: Window.Attribute) c_int;

pub fn setWindowAttribute(
    window: *Window,
    comptime attrib: Window.Attribute,
    value: Window.Attribute.ValueType(attrib),
) void {
    setWindowAttributeUntyped(window, cIntCast(attrib), value);
}
pub const setWindowAttributeUntyped = glfwSetWindowAttrib;
extern fn glfwSetWindowAttrib(window: *Window, attrib: Window.Attribute, value: c_int) void;

pub fn getWindowUserPointer(window: *Window, comptime T: type) ?*T {
    return @ptrCast(@alignCast(glfwGetWindowUserPointer(window)));
}
extern fn glfwGetWindowUserPointer(window: *Window) ?*anyopaque;

pub fn setWindowUserPointer(window: *Window, pointer: ?*anyopaque) void {
    glfwSetWindowUserPointer(window, pointer);
}
extern fn glfwSetWindowUserPointer(window: *Window, pointer: ?*anyopaque) void;

pub const setFramebufferSizeCallback = glfwSetFramebufferSizeCallback;
extern fn glfwSetFramebufferSizeCallback(*Window, ?FramebufferSizeFn) ?FramebufferSizeFn;
pub const FramebufferSizeFn = *const fn (*Window, width: c_int, height: c_int) callconv(.C) void;

pub const setWindowSizeCallback = glfwSetWindowSizeCallback;
extern fn glfwSetWindowSizeCallback(*Window, ?WindowSizeFn) ?WindowSizeFn;
pub const WindowSizeFn = *const fn (*Window, width: c_int, height: c_int) callconv(.C) void;

pub const setWindowPosCallback = glfwSetWindowPosCallback;
extern fn glfwSetWindowPosCallback(*Window, ?WindowPosFn) ?WindowPosFn;
pub const WindowPosFn = *const fn (*Window, x: c_int, y: c_int) callconv(.C) void;

pub const setWindowFocusCallback = glfwSetWindowFocusCallback;
extern fn glfwSetWindowFocusCallback(*Window, ?WindowFocusFn) ?WindowFocusFn;
pub const WindowFocusFn = *const fn (*Window, focused: Bool) callconv(.C) void;

pub const setWindowIconifyCallback = glfwSetWindowIconifyCallback;
extern fn glfwSetWindowIconifyCallback(*Window, ?IconifyFn) ?IconifyFn;
pub const IconifyFn = *const fn (*Window, iconified: Bool) callconv(.C) void;

pub const setWindowContentScaleCallback = glfwSetWindowContentScaleCallback;
extern fn glfwSetWindowContentScaleCallback(*Window, ?WindowContentScaleFn) ?WindowContentScaleFn;
pub const WindowContentScaleFn = *const fn (*Window, xscale: f32, yscale: f32) callconv(.C) void;

pub const setWindowCloseCallback = glfwSetWindowCloseCallback;
extern fn glfwSetWindowCloseCallback(*Window, ?WindowCloseFn) ?WindowCloseFn;
pub const WindowCloseFn = *const fn (*Window) callconv(.C) void;

pub const setKeyCallback = glfwSetKeyCallback;
extern fn glfwSetKeyCallback(*Window, ?KeyFn) ?KeyFn;
pub const KeyFn = *const fn (*Window, Key, scancode: c_int, Action, Mods) callconv(.C) void;

pub const setCharCallback = glfwSetCharCallback;
extern fn glfwSetCharCallback(*Window, ?CharFn) ?CharFn;
pub const CharFn = *const fn (*Window, codepoint: u32) callconv(.C) void;

pub const setDropCallback = glfwSetDropCallback;
extern fn glfwSetDropCallback(*Window, ?DropFn) ?DropFn;
pub const DropFn = *const fn (*Window, path_count: c_int, paths: [*][*:0]const u8) callconv(.C) void;

pub const setMouseButtonCallback = glfwSetMouseButtonCallback;
extern fn glfwSetMouseButtonCallback(*Window, ?MouseButtonFn) ?MouseButtonFn;
pub const MouseButtonFn = *const fn (*Window, MouseButton, Action, Mods) callconv(.C) void;

pub const setCursorPosCallback = glfwSetCursorPosCallback;
extern fn glfwSetCursorPosCallback(*Window, ?CursorPosFn) ?CursorPosFn;
pub const CursorPosFn = *const fn (*Window, xpos: f64, ypos: f64) callconv(.C) void;

pub const setScrollCallback = glfwSetScrollCallback;
extern fn glfwSetScrollCallback(*Window, ?ScrollFn) ?ScrollFn;
pub const ScrollFn = *const fn (*Window, xoffset: f64, yoffset: f64) callconv(.C) void;

pub const setCursorEnterCallback = glfwSetCursorEnterCallback;
extern fn glfwSetCursorEnterCallback(*Window, ?CursorEnterFn) ?CursorEnterFn;
pub const CursorEnterFn = *const fn (*Window, entered: Bool) callconv(.C) void;

pub const setWindowMonitor = glfwSetWindowMonitor;
extern fn glfwSetWindowMonitor(
    *Window,
    ?*Monitor,
    xpos: c_int,
    ypos: c_int,
    width: c_int,
    height: c_int,
    refreshRate: c_int,
) void;

pub const showWindow = glfwShowWindow;
extern fn glfwShowWindow(*Window) void;

pub const focusWindow = glfwFocusWindow;
extern fn glfwFocusWindow(*Window) void;

pub const getKey = glfwGetKey;
extern fn glfwGetKey(*Window, key: Key) Action;

pub const getMouseButton = glfwGetMouseButton;
extern fn glfwGetMouseButton(*Window, button: MouseButton) Action;

pub const getCursorPos = glfwGetCursorPos;
extern fn glfwGetCursorPos(*Window, xpos: ?*f64, ypos: ?*f64) void;

pub const setCursorPos = glfwSetCursorPos;
extern fn glfwSetCursorPos(*Window, xpos: f64, ypos: f64) void;

pub const setWindowSizeLimits = glfwSetWindowSizeLimits;
extern fn glfwSetWindowSizeLimits(*Window, min_w: c_int, min_h: c_int, max_w: c_int, max_h: c_int) void;

pub const getWindowContentScale = glfwGetWindowContentScale;
extern fn glfwGetWindowContentScale(*Window, xscale: ?*f32, yscale: ?*f32) void;

pub const getFramebufferSize = glfwGetFramebufferSize;
extern fn glfwGetFramebufferSize(*Window, width: ?*c_int, height: ?*c_int) void;

pub const getWindowSize = glfwGetWindowSize;
extern fn glfwGetWindowSize(*Window, width: ?*c_int, height: ?*c_int) void;

pub const setSize = glfwSetWindowSize;
extern fn glfwSetWindowSize(*Window, width: c_int, height: c_int) void;

pub const getWindowPos = glfwGetWindowPos;
extern fn glfwGetWindowPos(*Window, xpos: *c_int, ypos: *c_int) void;

pub const setWindowPos = glfwSetWindowPos;
extern fn glfwSetWindowPos(*Window, xpos: i32, ypos: i32) void;

pub fn setWindowTitle(window: *Window, title: [:0]const u8) void {
    glfwSetWindowTitle(window, title);
}
extern fn glfwSetWindowTitle(*Window, title: [*:0]const u8) void;

pub fn setWindowIcon(window: *Window, images: []const Image) void {
    glfwSetWindowIcon(window, @intCast(images.len), images.ptr);
}
extern fn glfwSetWindowIcon(*Window, count: c_int, images: [*]const Image) void;

pub fn windowShouldClose(window: *Window) bool {
    return glfwWindowShouldClose(window) == TRUE;
}
extern fn glfwWindowShouldClose(window: *Window) Bool;

pub fn setWindowShouldClose(window: *Window, should_close: bool) void {
    return glfwSetWindowShouldClose(window, if (should_close) TRUE else FALSE);
}
extern fn glfwSetWindowShouldClose(*Window, should_close: Bool) void;

pub const setCursor = glfwSetCursor;
extern fn glfwSetCursor(*Window, ?*Cursor) void;

pub const InputMode = enum(c_int) {
    cursor = 0x00033001,
    sticky_keys = 0x00033002,
    sticky_mouse_buttons = 0x00033003,
    lock_key_mods = 0x00033004,
    raw_mouse_motion = 0x00033005,

    pub fn ValueType(comptime mode: InputMode) type {
        return switch (mode) {
            .cursor => Cursor.Mode,
            else => bool,
        };
    }
};
pub fn setInputMode(
    window: *Window,
    comptime mode: InputMode,
    value: InputMode.ValueType(mode),
) Error!void {
    try setInputModeUntyped(window, mode, value);
}
pub fn setInputModeUntyped(window: *Window, mode: InputMode, value: anytype) Error!void {
    glfwSetInputMode(window, mode, cIntCast(value));
    try maybeError();
}
extern fn glfwSetInputMode(*Window, InputMode, value: c_int) void;

pub const swapBuffers = glfwSwapBuffers;
extern fn glfwSwapBuffers(*Window) void;

pub const WindowHint = enum(c_int) {
    focused = 0x00020001,
    iconified = 0x00020002,
    resizable = 0x00020003,
    visible = 0x00020004,
    decorated = 0x00020005,
    auto_iconify = 0x00020006,
    floating = 0x00020007,
    maximized = 0x00020008,
    center_cursor = 0x00020009,
    transparent_framebuffer = 0x0002000A,
    hovered = 0x0002000B,
    focus_on_show = 0x0002000C,
    mouse_passthrough = 0x0002000D,
    position_x = 0x0002000E,
    position_y = 0x0002000F,
    red_bits = 0x00021001,
    green_bits = 0x00021002,
    blue_bits = 0x00021003,
    alpha_bits = 0x00021004,
    depth_bits = 0x00021005,
    stencil_bits = 0x00021006,
    // ACCUM_*_BITS/AUX_BUFFERS are deprecated
    stereo = 0x0002100C,
    samples = 0x0002100D,
    srgb_capable = 0x0002100E,
    refresh_rate = 0x0002100F,
    doublebuffer = 0x00021010,
    client_api = 0x00022001,
    context_version_major = 0x00022002,
    context_version_minor = 0x00022003,
    context_revision = 0x00022004,
    context_robustness = 0x00022005,
    opengl_forward_compat = 0x00022006,
    opengl_debug_context = 0x00022007,
    opengl_profile = 0x00022008,
    context_release_behaviour = 0x00022009,
    context_no_error = 0x0002200A,
    context_creation_api = 0x0002200B,
    scale_to_monitor = 0x0002200C,
    scale_framebuffer = 0x0002200D,
    cocoa_retina_framebuffer = 0x00023001,
    cocoa_frame_name = 0x00023002,
    cocoa_graphics_switching = 0x00023003,
    x11_class_name = 0x00024001,
    x11_instance_name = 0x00024002,
    win32_keyboard_menu = 0x00025001,
    win32_showdefault = 0x00025002,
    wayland_app_id = 0x00026001,

    fn ValueType(comptime window_hint: WindowHint) type {
        return switch (window_hint) {
            .focused,
            .iconified,
            .resizable,
            .visible,
            .decorated,
            .auto_iconify,
            .floating,
            .maximized,
            .center_cursor,
            .transparent_framebuffer,
            .hovered,
            .focus_on_show,
            .mouse_passthrough,
            => bool,
            .position_x, .position_y => c_int,
            .red_bits, .green_bits, .blue_bits, .alpha_bits, .depth_bits, .stencil_bits => c_int,
            .stereo => bool,
            .samples => c_int,
            .srgb_capable => bool,
            .refresh_rate => c_int,
            .doublebuffer => bool,
            .client_api => ClientApi,
            .context_version_major, .context_version_minor, .context_revision => c_int,
            .context_robustness => ContextRobustness,
            .opengl_forward_compat, .opengl_debug_context => bool,
            .opengl_profile => OpenGLProfile,
            .context_release_behaviour => ReleaseBehaviour,
            .context_no_error => bool,
            .context_creation_api => ContextCreationApi,
            .scale_to_monitor, .scale_framebuffer, .cocoa_retina_framebuffer => bool,
            .cocoa_frame_name => [:0]const u8,
            .cocoa_graphics_switching => bool,
            .x11_class_name, .x11_instance_name => [:0]const u8,
            .win32_keyboard_menu, .win32_showdefault => bool,
            .wayland_app_id => [:0]const u8,
        };
    }

    pub const set = windowHint;
};

pub fn windowHint(comptime hint: WindowHint, value: WindowHint.ValueType(hint)) void {
    const ValueType = @TypeOf(value);
    switch (ValueType) {
        [:0]const u8 => windowHintString(hint, value),
        else => windowHintUntyped(hint, cIntCast(value)),
    }
}
pub const windowHintUntyped = glfwWindowHint;
extern fn glfwWindowHint(WindowHint, value: c_int) void;

pub fn windowHintString(hint: WindowHint, string: [:0]const u8) void {
    glfwWindowHintString(hint, string);
}
extern fn glfwWindowHintString(WindowHint, [*:0]const u8) void;

pub const ClientApi = enum(c_int) {
    no_api = 0,
    opengl_api = 0x00030001,
    opengl_es_api = 0x00030002,
};

pub const OpenGLProfile = enum(c_int) {
    opengl_any_profile = 0,
    opengl_core_profile = 0x00032001,
    opengl_compat_profile = 0x00032002,
};

pub const ContextRobustness = enum(c_int) {
    no_robustness = 0,
    no_reset_notification = 0x00031001,
    lose_context_on_reset = 0x00031002,
};

pub const ReleaseBehaviour = enum(c_int) {
    any = 0,
    flush = 0x00035001,
    none = 0x00035002,
};

pub const ContextCreationApi = enum(c_int) {
    native = 0x00036001,
    egl = 0x00036002,
    osmesa = 0x00036003,
};

//--------------------------------------------------------------------------------------------------
//
// Native
//
//--------------------------------------------------------------------------------------------------
pub const getWin32Adapter = if (builtin.target.os.tag == .windows) glfwGetWin32Adapter else _getWin32Adapter;
extern fn glfwGetWin32Adapter(*Monitor) ?[*:0]const u8;
fn _getWin32Adapter(_: *Monitor) ?[*:0]const u8 {
    return null;
}

pub const getWin32Window = if (builtin.target.os.tag == .windows) glfwGetWin32Window else _getWin32Window;
extern fn glfwGetWin32Window(*Window) ?std.os.windows.HWND;
fn _getWin32Window(_: *Window) ?std.os.windows.HWND {
    return null;
}

pub const getX11Adapter = if (_isLinuxDesktopLike() and options.enable_x11) glfwGetX11Adapter else _getX11Adapter;
extern fn glfwGetX11Adapter(*Monitor) u32;
fn _getX11Adapter(_: *Monitor) u32 {
    return 0;
}

pub const getX11Display = if (_isLinuxDesktopLike() and options.enable_x11) glfwGetX11Display else _getX11Display;
extern fn glfwGetX11Display() ?*anyopaque;
fn _getX11Display() ?*anyopaque {
    return null;
}

pub const getX11Window = if (_isLinuxDesktopLike() and options.enable_x11) glfwGetX11Window else _getX11Window;
extern fn glfwGetX11Window(window: *Window) u32;
fn _getX11Window(_: *Window) u32 {
    return 0;
}

pub const getWaylandDisplay = if (_isLinuxDesktopLike() and options.enable_wayland) glfwGetWaylandDisplay else _getWaylandDisplay;
extern fn glfwGetWaylandDisplay() ?*anyopaque;
fn _getWaylandDisplay() ?*anyopaque {
    return null;
}

pub const getWaylandWindow = if (_isLinuxDesktopLike() and options.enable_wayland) glfwGetWaylandWindow else _getWaylandWindow;
extern fn glfwGetWaylandWindow(window: *Window) ?*anyopaque;
fn _getWaylandWindow(_: *Window) ?*anyopaque {
    return null;
}

pub const getCocoaWindow = if (builtin.target.os.tag == .macos) glfwGetCocoaWindow else _getCocoaWindow;
extern fn glfwGetCocoaWindow(window: *Window) ?*anyopaque;
fn _getCocoaWindow(_: *Window) ?*anyopaque {
    return null;
}

fn _isLinuxDesktopLike() bool {
    return switch (builtin.target.os.tag) {
        .linux,
        .freebsd,
        .openbsd,
        .dragonfly,
        => true,
        else => false,
    };
}

//--------------------------------------------------------------------------------------------------
//
// Emscripten
//
//--------------------------------------------------------------------------------------------------
const os = builtin.target.os.tag;
usingnamespace if (os == .emscripten or os == .freestanding) struct {
    // GLFW - emscripten uses older version that doesn't have these functions - implement dummies
    var glfwGetGamepadStateWarnPrinted: bool = false;
    pub export fn glfwGetGamepadState(_: i32, _: ?*anyopaque) i32 {
        if (!glfwGetGamepadStateWarnPrinted) {
            std.log.err("glfwGetGamepadState(): not implemented! Use emscripten specific functions: https://emscripten.org/docs/api_reference/html5.h.html?highlight=gamepadstate#c.emscripten_get_gamepad_status", .{});
            glfwGetGamepadStateWarnPrinted = true;
        }
        return 0; // false - failure
    }

    /// use glfwSetCallback instead
    pub export fn glfwGetError() i32 {
        return 0; // no error
    }
} else struct {};
