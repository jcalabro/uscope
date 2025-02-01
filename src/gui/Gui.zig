const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const assert = std.debug.assert;
const fs = std.fs;
const mem = std.mem;
const time = std.time;

const cimgui = @import("cimgui");
const gl = @import("zopengl");
const glfw = @import("zglfw");
const ztime = @import("time");

const stbi = @cImport({
    @cInclude("stb_image.h");
});

const BreakpointView = @import("views/Breakpoint.zig");
const debugger = @import("../debugger.zig");
const Debugger = debugger.Debugger;
const FilePickerView = @import("views/FilePicker.zig");
const flags = @import("../flags.zig");
const Input = @import("Input.zig");
const logging = @import("../logging.zig");
const PrimaryView = @import("views/Primary.zig");
const proto = debugger.proto;
const settings = @import("../settings.zig");
const State = @import("State.zig");
const trace = @import("../trace.zig");
const types = @import("../types.zig");
const zui = @import("zui.zig");

const imgui = cimgui.c;
const log = logging.Logger.init(logging.Region.GUI);

const font_ttf = @embedFile("fonts/NotoMono/NotoMono-Regular.ttf");
const emoji_font_ttf = @embedFile("fonts/Noto/NotoEmoji-VariableFont_wght.ttf");
const icon_png = @embedFile("images/icon.png");

// @TODO (jrc): inherit this from the monitor on which we're rendering
pub const MaxFPS: u64 = 60;
const FrameMicros: u64 = @divFloor(time.us_per_s, MaxFPS);

const Gui = @This();

perm_alloc: Allocator,

/// [0] == x, [1] == y
window_size: @Vector(2, i32) = .{ 0, 0 },
window_needs_resize: bool = false,

window: *glfw.Window,

last_frame_render_micros: u64 = 0,

main_dockspace_id: zui.ID = undefined,

state: *State = undefined,

fn init(alloc: Allocator, dbg: *Debugger) !*Gui {
    const z = trace.zoneN(@src(), "GUI.init");
    defer z.end();

    zui.init(alloc);

    try glfw.init();

    // @TODO(jrc): get the monitor on which the window will
    // be opened, not just the primary monitor
    const monitor = glfw.Monitor.getPrimary();
    if (monitor == null) {
        log.err("unable to get primary monitor");
    }
    const mode = try glfw.Monitor.getVideoMode(monitor.?);

    const width = 2000;
    const height = 1200;
    const x_pos = @divFloor(mode.width - width, 2);
    const y_pos = @divFloor(mode.height - height, 2);

    const gl_major = 4;
    const gl_minor = 0;
    glfw.windowHint(.context_version_major, gl_major);
    glfw.windowHint(.context_version_minor, gl_minor);
    glfw.windowHint(.opengl_profile, .opengl_core_profile);
    glfw.windowHint(.opengl_forward_compat, true);
    glfw.windowHint(.client_api, .opengl_api);
    glfw.windowHint(.doublebuffer, true);

    const window = try glfw.Window.create(width, height, "uscope", null);
    glfw.Window.setPos(window, x_pos, y_pos);
    glfw.makeContextCurrent(window);

    {
        // set the window icon
        var w: i32 = 0;
        var h: i32 = 0;
        var n: i32 = 0;
        const icon_pixels = stbi.stbi_load_from_memory(icon_png, icon_png.len, &w, &h, &n, 4);
        defer stbi.stbi_image_free(icon_pixels);

        const icons = try alloc.alloc(glfw.Image, 1);
        defer alloc.free(icons);

        icons[0] = glfw.Image{
            .width = w,
            .height = h,
            .pixels = icon_pixels,
        };
        window.setIcon(icons);
    }

    try gl.loadCoreProfile(glfw.getProcAddress, gl_major, gl_minor);

    // @TODO (jrc): figure out the correct way to detect if the device supports
    // vsync. swapInterval(1) enables vsync, swapInterval(0) disables it. If vsync
    // is disabled, we should lock our max framerate manually. This should probably
    // come from a user setting, or we could just always disable since we're not
    // doing anything super graphically intense.
    glfw.swapInterval(0);

    cimgui.init(alloc);

    const io: *imgui.ImGuiIO = imgui.igGetIO();
    io.ConfigFlags |= imgui.ImGuiConfigFlags_DockingEnable;
    io.ConfigFlags |= imgui.ImGuiConfigFlags_IsSRGB;

    // Disable the ini file to save layout
    io.LogFilename = null;

    cimgui.backend.initWithGlSlVersion(window, "#version 130");

    var cfg_dir = try settings.globalConfigDir(alloc);
    defer cfg_dir.deinit();
    const file_name = "gui.ini";

    if (flags.ResetGUI) blk: {
        var dir = fs.openDirAbsolute(cfg_dir.items, .{}) catch {
            // directory does not exist
            break :blk;
        };
        defer dir.close();

        dir.deleteFile(file_name) catch {};
    }

    try cfg_dir.appendSlice(file_name);
    io.IniFilename = try cfg_dir.toOwnedSliceSentinel(0);

    //
    // @TODO (jrc): allow the user to configure their font size
    // in their global settings
    // @TODO (jrc): This will have to be recalculated for different screen DPIs
    // @TODO (jrc): load fonts for other glyph ranges
    //

    const font_size = 20;

    {
        // load the default font
        const font_config: *imgui.ImFontConfig = imgui.ImFontConfig_ImFontConfig();
        defer imgui.ImFontConfig_destroy(font_config);

        font_config.FontDataOwnedByAtlas = false;
        font_config.FontBuilderFlags |= imgui.ImGuiFreeTypeBuilderFlags_LoadColor;

        _ = imgui.ImFontAtlas_AddFontFromMemoryTTF(
            io.Fonts,
            @constCast(@ptrCast(font_ttf)),
            font_ttf.len,
            font_size,
            font_config,
            null,
        );
    }

    {
        // load a font containing many common emojis
        const font_config: *imgui.ImFontConfig = imgui.ImFontConfig_ImFontConfig();
        defer imgui.ImFontConfig_destroy(font_config);

        font_config.OversampleH = 1;
        font_config.OversampleV = 1;
        font_config.MergeMode = true;

        font_config.FontDataOwnedByAtlas = false;
        font_config.FontBuilderFlags |= imgui.ImGuiFreeTypeBuilderFlags_LoadColor;

        const ranges = [_]imgui.ImWchar{ 0x1, 0x1FFFF, 0 };

        _ = imgui.ImFontAtlas_AddFontFromMemoryTTF(
            io.Fonts,
            @constCast(@ptrCast(emoji_font_ttf)),
            emoji_font_ttf.len,
            font_size,
            font_config,
            &ranges,
        );
    }

    setImGUIStyle();

    _ = window.setKeyCallback(Input.keyCallback);

    const self = try alloc.create(Gui);
    errdefer alloc.destroy(self);

    self.* = Gui{
        .perm_alloc = alloc,
        .window = window,
    };

    if (!builtin.is_test) {
        self.state = try State.init(alloc, dbg, self);
        self.state.loadDebugSymbols();
    }

    window.setUserPointer(self);
    _ = glfw.Window.setSizeCallback(window, windowResizedCallback);
    self.setWindowSize(window);

    return self;
}

fn deinit(self: *Gui) void {
    const z = trace.zone(@src());
    defer z.end();

    self.state.deinit();
    Input.deinit();

    cimgui.deinit();
    self.window.destroy();

    glfw.terminate();

    zui.deinit();

    self.perm_alloc.destroy(self);
}

pub fn run(alloc: Allocator, dbg: *Debugger) !void {
    const z = trace.zoneN(@src(), "gui.run");
    defer z.end();

    const self = try init(alloc, dbg);
    defer self.deinit();

    while (!self.window.shouldClose() and !self.state.shutting_down) {
        const zf = trace.zoneN(@src(), "frame");
        defer zf.end();

        glfw.pollEvents();

        if (self.window_needs_resize) {
            // the window was resized in the callback, adjust the imgui buffer
            self.window_needs_resize = false;

            zui.setNextWindowSize(
                @floatFromInt(self.window_size[0]),
                @floatFromInt(self.window_size[1]),
            );
        }

        cimgui.backend.newFrame(
            @intCast(self.window_size[0]),
            @intCast(self.window_size[1]),
        );

        if (flags.ImGuiDemo) {
            var open = true;
            imgui.igShowDemoWindow(&open);

            if (Input.keyPressedWithCtrl(.q)) {
                self.state.shutting_down = true;
            }
        } else {
            self.main_dockspace_id = zui.dockSpaceOverViewport(.{});
            if (self.state.active_view == .primary) {
                if (self.drawMenuBar()) |view| {
                    self.state.active_view = view;
                }
            }

            self.state.update();
        }

        self.render();
        self.last_frame_render_micros = frameRateLimit(self.last_frame_render_micros);
    }

    self.state.quit();
}

/// Locks us to our maximum frame limit
pub fn frameRateLimit(last_frame_render_micros: u64) u64 {
    const z = trace.zone(@src());
    defer z.end();

    const now: u64 = @intCast(time.microTimestamp());
    const frame_duration = now - last_frame_render_micros;
    if (frame_duration < FrameMicros) {
        const wait_for = FrameMicros - frame_duration;
        time.sleep(wait_for * time.ns_per_us);
    }

    return @intCast(time.microTimestamp());
}

fn render(self: *Gui) void {
    const z = trace.zone(@src());
    defer z.end();

    cimgui.backend.draw();
    self.window.swapBuffers();
}

fn setWindowSize(self: *Gui, window: *glfw.Window) void {
    const z = trace.zone(@src());
    defer z.end();

    //
    // @TODO (jrc): we should be able to gracefully handle the case
    // where a monitor is plugged in or unplugged (need to double-check
    // how this code behaves in that scenario)
    //

    const size = window.getSize();
    self.window_size[0] = size[0];
    self.window_size[1] = size[1];
    self.window_needs_resize = true;

    log.debugf("window resized to {d}x{d}", .{
        self.window_size[0],
        self.window_size[1],
    });
}

fn windowResizedCallback(
    window: *glfw.Window,
    width: i32,
    height: i32,
) callconv(.C) void {
    _ = width;
    _ = height;
    const z = trace.zone(@src());
    defer z.end();

    const gui_null = window.getUserPointer(Gui);
    assert(gui_null != null);
    gui_null.?.setWindowSize(window);
}

pub const WindowSize = struct {
    x: f32,
    y: f32,

    w: f32,
    h: f32,
};

pub fn getSingleFocusWindowSize(self: Gui) WindowSize {
    return self.getSingleFocusWindowSizeWithScale(0.95);
}

/// returns the width and height of a window that is almost full-screen,
/// but has a bit of border padding around the edges
pub fn getSingleFocusWindowSizeWithScale(self: Gui, scale: f32) WindowSize {
    assert(scale >= 0);
    assert(scale <= 1);

    const size = self.window.getSize();
    const w: f32 = @floatFromInt(size[0]);
    const h: f32 = @floatFromInt(size[1]);

    const w_scaled = w * scale;
    const h_scaled = h * scale;

    const x = (w - w_scaled) / 2.0;
    const y = (h - h_scaled) / 2.0;

    return .{
        .x = x,
        .y = y,
        .w = w_scaled,
        .h = h_scaled,
    };
}

pub fn getMainDockspaceID(self: Gui) zui.ID {
    return self.main_dockspace_id;
}

fn drawMenuBar(self: *Gui) ?State.View {
    const z = trace.zone(@src());
    defer z.end();

    if (zui.beginMainMenuBar()) {
        defer zui.endMainMenuBar();

        if (zui.beginMenu("File", true)) {
            defer zui.endMenu();

            if (zui.menuItem("Open File", .{
                .shortcut = "space+f",
            })) {
                return self.state.file_picker.view();
            }

            if (zui.menuItem("Close File", .{
                .shortcut = "ctrl+d",
                .enabled = self.state.open_files.items.len > 0,
            })) {
                self.state.closeSourceFile(null);
            }

            if (zui.menuItem("View Breakpoints", .{
                .shortcut = "space+b",
            })) {
                return self.state.breakpoint.view();
            }

            if (zui.menuItem("Quit", .{ .shortcut = "ctrl+q" })) {
                self.state.quit();
            }
        }

        if (zui.beginMenu("Debugging", true)) {
            defer zui.endMenu();

            // @TODO (jrc): add more flow controls to the menu

            switch (self.state.dbg_state.paused == null) {
                true => {
                    if (zui.menuItem("Run", .{ .shortcut = "r" })) {
                        self.state.launchSubordinate();
                    }
                },
                false => {
                    if (zui.menuItem("Kill", .{ .shortcut = "k" })) self.state.killSubordinate();
                    if (zui.menuItem("Continue", .{ .shortcut = "c" })) self.state.continueExecution();
                    if (zui.menuItem("Step Out", .{ .shortcut = "w" })) self.state.sendStepRequest(.out_of);
                    if (zui.menuItem("Single Step", .{ .shortcut = "a" })) self.state.sendStepRequest(.single);
                    if (zui.menuItem("Step Into", .{ .shortcut = "s" })) self.state.sendStepRequest(.into);
                    if (zui.menuItem("Step Over", .{ .shortcut = "d" })) self.state.sendStepRequest(.over);
                },
            }
        }

        if (zui.beginMenu("View", true)) blk: {
            defer zui.endMenu();

            // @TODO (jrc): serialize the user's open window preferences to their global settings file

            inline for (@typeInfo(PrimaryView.OpenWindows).@"struct".fields) |field| {
                var name = self.state.scratch_alloc.alloc(u8, field.name.len + 1) catch |err| {
                    log.errf("unable to alloc temp storage for view name: {!}", .{err});
                    break :blk;
                };
                @memset(name, 0);
                mem.copyForwards(u8, name, field.name);
                name[0] = std.ascii.toUpper(name[0]);

                if (zui.menuItem(@ptrCast(name), .{})) {
                    @field(self.state.primary.open_windows, field.name) = !@field(self.state.primary.open_windows, field.name);
                }
            }
        }

        // if (zui.beginMenu("Advanced", true)) {
        //     defer zui.endMenu();

        //     if (zui.menuItem("Diagnostics", .{})) {
        //         self.state.primary.toggleDiagnosticsView();
        //     }
        // }

        if (zui.beginMenu("About", true)) {
            defer zui.endMenu();

            var year = ztime.DateTime.now().years;
            if (year < 2024) year = 2024;

            zui.text("Thank you for using uscope! This software is in early alpha, so there will be bugs.", .{});
            zui.text("", .{});
            zui.text("Please reach out on Discord or submit a GitHub issue if you have encountered a bug.", .{});
            zui.text("", .{});
            zui.text("© {d}", .{year});
        }
    }

    return null;
}

/// https://github.com/ocornut/imgui/issues/707#issuecomment-917151020
fn setImGUIStyle() void {
    var style = zui.getStyle();
    style.colors[zui.StyleCol.text.int()] = .{ 1.00, 1.00, 1.00, 1.00 };
    style.colors[zui.StyleCol.text_disabled.int()] = .{ 0.50, 0.50, 0.50, 1.00 };
    style.colors[zui.StyleCol.window_bg.int()] = .{ 0.10, 0.10, 0.10, 1.00 };
    style.colors[zui.StyleCol.child_bg.int()] = .{ 0.00, 0.00, 0.00, 0.00 };
    style.colors[zui.StyleCol.popup_bg.int()] = .{ 0.19, 0.19, 0.19, 0.92 };
    style.colors[zui.StyleCol.border.int()] = .{ 0.19, 0.19, 0.19, 0.29 };
    style.colors[zui.StyleCol.border_shadow.int()] = .{ 0.00, 0.00, 0.00, 0.24 };
    style.colors[zui.StyleCol.frame_bg.int()] = .{ 0.05, 0.05, 0.05, 0.54 };
    style.colors[zui.StyleCol.frame_bg_hovered.int()] = .{ 0.19, 0.19, 0.19, 0.54 };
    style.colors[zui.StyleCol.frame_bg_active.int()] = .{ 0.20, 0.22, 0.23, 1.00 };
    style.colors[zui.StyleCol.title_bg.int()] = .{ 0.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.title_bg_active.int()] = .{ 0.06, 0.06, 0.06, 1.00 };
    style.colors[zui.StyleCol.title_bg_collapsed.int()] = .{ 0.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.menu_bar_bg.int()] = .{ 0.14, 0.14, 0.14, 1.00 };
    style.colors[zui.StyleCol.scrollbar_bg.int()] = .{ 0.05, 0.05, 0.05, 0.54 };
    style.colors[zui.StyleCol.scrollbar_grab.int()] = .{ 0.34, 0.34, 0.34, 0.54 };
    style.colors[zui.StyleCol.scrollbar_grab_hovered.int()] = .{ 0.40, 0.40, 0.40, 0.54 };
    style.colors[zui.StyleCol.scrollbar_grab_active.int()] = .{ 0.56, 0.56, 0.56, 0.54 };
    style.colors[zui.StyleCol.check_mark.int()] = .{ 0.33, 0.67, 0.86, 1.00 };
    style.colors[zui.StyleCol.slider_grab.int()] = .{ 0.34, 0.34, 0.34, 0.54 };
    style.colors[zui.StyleCol.slider_grab_active.int()] = .{ 0.56, 0.56, 0.56, 0.54 };
    style.colors[zui.StyleCol.button.int()] = .{ 0.05, 0.05, 0.05, 0.54 };
    style.colors[zui.StyleCol.button_hovered.int()] = .{ 0.19, 0.19, 0.19, 0.54 };
    style.colors[zui.StyleCol.button_active.int()] = .{ 0.20, 0.22, 0.23, 1.00 };
    style.colors[zui.StyleCol.header.int()] = .{ 0.00, 0.00, 0.00, 0.52 };
    style.colors[zui.StyleCol.header_hovered.int()] = .{ 0.00, 0.00, 0.00, 0.36 };
    style.colors[zui.StyleCol.header_active.int()] = .{ 0.20, 0.22, 0.23, 0.33 };
    style.colors[zui.StyleCol.separator.int()] = .{ 0.28, 0.28, 0.28, 0.29 };
    style.colors[zui.StyleCol.separator_hovered.int()] = .{ 0.44, 0.44, 0.44, 0.29 };
    style.colors[zui.StyleCol.separator_active.int()] = .{ 0.40, 0.44, 0.47, 1.00 };
    style.colors[zui.StyleCol.resize_grip.int()] = .{ 0.28, 0.28, 0.28, 0.29 };
    style.colors[zui.StyleCol.resize_grip_hovered.int()] = .{ 0.44, 0.44, 0.44, 0.29 };
    style.colors[zui.StyleCol.resize_grip_active.int()] = .{ 0.40, 0.44, 0.47, 1.00 };
    style.colors[zui.StyleCol.tab.int()] = .{ 0.00, 0.00, 0.00, 0.52 };
    style.colors[zui.StyleCol.tab_hovered.int()] = .{ 0.14, 0.14, 0.14, 1.00 };
    style.colors[zui.StyleCol.tab_active.int()] = .{ 0.20, 0.20, 0.20, 0.36 };
    style.colors[zui.StyleCol.tab_unfocused.int()] = .{ 0.00, 0.00, 0.00, 0.52 };
    style.colors[zui.StyleCol.tab_unfocused_active.int()] = .{ 0.14, 0.14, 0.14, 1.00 };
    style.colors[zui.StyleCol.docking_preview.int()] = .{ 0.33, 0.67, 0.86, 1.00 };
    style.colors[zui.StyleCol.docking_empty_bg.int()] = .{ 0.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.plot_lines.int()] = .{ 1.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.plot_lines_hovered.int()] = .{ 1.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.plot_histogram.int()] = .{ 1.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.plot_histogram_hovered.int()] = .{ 1.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.table_header_bg.int()] = .{ 0.00, 0.00, 0.00, 0.52 };
    style.colors[zui.StyleCol.table_border_strong.int()] = .{ 0.00, 0.00, 0.00, 0.52 };
    style.colors[zui.StyleCol.table_border_light.int()] = .{ 0.28, 0.28, 0.28, 0.29 };
    style.colors[zui.StyleCol.table_row_bg.int()] = .{ 0.00, 0.00, 0.00, 0.00 };
    style.colors[zui.StyleCol.table_row_bg_alt.int()] = .{ 1.00, 1.00, 1.00, 0.06 };
    style.colors[zui.StyleCol.text_selected_bg.int()] = .{ 0.20, 0.22, 0.23, 1.00 };
    style.colors[zui.StyleCol.drag_drop_target.int()] = .{ 0.33, 0.67, 0.86, 1.00 };
    style.colors[zui.StyleCol.nav_highlight.int()] = .{ 1.00, 0.00, 0.00, 1.00 };
    style.colors[zui.StyleCol.nav_windowing_highlight.int()] = .{ 1.00, 0.00, 0.00, 0.70 };
    style.colors[zui.StyleCol.nav_windowing_dim_bg.int()] = .{ 1.00, 0.00, 0.00, 0.20 };
    style.colors[zui.StyleCol.modal_window_dim_bg.int()] = .{ 0.00, 0.00, 0.00, 0.35 };

    style.window_padding = .{ .x = 8.00, .y = 8.00 };
    style.frame_padding = .{ .x = 5.00, .y = 2.00 };
    style.cell_padding = .{ .x = 6.00, .y = 6.00 };
    style.item_spacing = .{ .x = 6.00, .y = 6.00 };
    style.item_inner_spacing = .{ .x = 6.00, .y = 6.00 };
    style.touch_extra_padding = .{ .x = 0.00, .y = 0.00 };
    style.indent_spacing = 25;
    style.scrollbar_size = 15;
    style.grab_min_size = 10;
    style.window_border_size = 1;
    style.child_border_size = 1;
    style.popup_border_size = 1;
    style.frame_border_size = 1;
    style.tab_border_size = 1;
    style.window_rounding = 7;
    style.child_rounding = 4;
    style.frame_rounding = 3;
    style.popup_rounding = 4;
    style.scrollbar_rounding = 9;
    style.grab_rounding = 3;
    style.log_slider_deadzone = 4;
    style.tab_rounding = 4;
}
