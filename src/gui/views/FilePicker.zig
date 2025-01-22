const std = @import("std");
const ascii = std.ascii;
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const fs = std.fs;
const mem = std.mem;

const file = @import("../../file.zig");
const GUI = @import("../GUI.zig");
const Input = @import("../Input.zig");
const logging = @import("../../logging.zig");
const State = @import("../State.zig");
const trace = @import("../../trace.zig");
const windows = @import("../windows.zig");
const zui = @import("../zui.zig");

const cimgui = @import("cimgui");
const imgui = cimgui.c;

const log = logging.Logger.init(logging.Region.GUI);

const Self = @This();

gui: *State.GUIType,
state: *State,

hide_mouse: bool = true,
cursor_ndx: usize = 0,

file_paths: ArrayList([]const u8),
file_previews: ArrayList(FilePreview),

/// when there is no filter text, all file_path ndx'es are present
selected_files: ArrayList(usize),

// @TODO (jrc): make these configurable via global settings
const skipList = &.{
    ".", // hidden files
    "zig-cache",
    "zig-out",
    "build",
};

const rustSkipList = &.{
    "target/debug/",
    "target/release/",
};

const skipExtensions = &.{
    ".o",
    ".a",
    ".obj",
    ".ttf",
    ".bin",

    // @DELETEME (jrc): this is specific to microscope itself
    "out",
};

const FilePreview = struct {
    const PreviewSize = 1024 * 2;

    /// preview contains the first N bytes of the file. It should not be
    /// accessed directly; instead, we lazy load using getPreview().
    preview: ?[]const u8 = null,

    fn getPreview(self: *@This(), state: *State, filePath: []const u8) ![]const u8 {
        if (self.preview) |p| return p;

        assert(filePath.len > 0);
        const fp = try file.open(filePath, .{ .mode = .read_only });
        defer fp.close();

        const contents = try file.readWholeFile(state.scratch_alloc, fp);
        defer state.scratch_alloc.free(contents);

        const size = @min(contents.len, PreviewSize);
        const preview = try state.perm_alloc.alloc(u8, size);
        errdefer state.perm_alloc.destroy(preview);

        @memcpy(preview, contents[0..size]);
        self.preview = preview;
        return preview;
    }
};

pub fn init(state: *State, gui: *State.GUIType) !*Self {
    const z = trace.zone(@src());
    defer z.end();

    const self = try state.perm_alloc.create(Self);
    errdefer state.perm_alloc.destroy(self);

    self.* = .{
        .state = state,
        .gui = gui,
        .file_paths = ArrayList([]const u8).init(state.perm_alloc),
        .file_previews = ArrayList(FilePreview).init(state.perm_alloc),
        .selected_files = ArrayList(usize).init(state.perm_alloc),
    };

    try self.reset();
    return self;
}

fn clearFiles(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    for (self.file_paths.items) |it| {
        self.state.perm_alloc.free(it);
    }
    self.file_paths.clearAndFree();

    for (self.file_previews.items) |it| {
        if (it.preview) |p| self.state.perm_alloc.free(p);
    }
    self.file_previews.clearAndFree();

    self.selected_files.clearAndFree();
}

fn reset(self: *Self) !void {
    const z = trace.zone(@src());
    defer z.end();

    //
    // Recursively walk the directory tree from the cwd
    // and find all files that we would potentially want
    // to open in the source viewer in the debugger
    //

    self.clearFiles();

    var dirs = ArrayList([]const u8).init(self.state.perm_alloc);
    defer {
        for (dirs.items) |item| self.state.perm_alloc.free(item);
        defer dirs.deinit();
    }

    const cwd = fs.cwd();
    var iterableDir = try cwd.openDir(".", .{ .iterate = true });
    defer iterableDir.close();

    var walker = try iterableDir.walk(self.state.perm_alloc);
    while (try walker.next()) |entry| {
        if (entry.path.len == 0) continue;

        var skip = false;
        inline for (skipList) |s| {
            if (mem.startsWith(u8, entry.path, s)) {
                skip = true;
            }
        }
        inline for (rustSkipList) |s| {
            if (mem.indexOf(u8, entry.path, s) != null) {
                skip = true;
            }
        }
        inline for (skipExtensions) |s| {
            if (mem.endsWith(u8, entry.path, s)) {
                skip = true;
            }
        }
        if (skip) continue;

        switch (entry.kind) {
            .file => {
                const str = try self.state.perm_alloc.alloc(u8, entry.path.len);
                @memcpy(str, entry.path);
                try self.file_paths.append(str);
                try self.file_previews.append(.{});
            },
            .directory => {
                const absPath = try cwd.realpathAlloc(self.state.perm_alloc, entry.path);
                errdefer self.state.perm_alloc.free(absPath);

                try dirs.append(absPath);
            },
            else => {}, // skip other file system entries
        }
    }

    try self.filter("");
}

fn getFilteredPaths(self: Self) ![][]const u8 {
    const z = trace.zone(@src());
    defer z.end();

    var files = ArrayList([]const u8).init(self.state.perm_alloc);
    errdefer files.deinit();

    for (self.selected_files.items) |ndx| {
        try files.append(self.file_paths.items[ndx]);
    }

    return try files.toOwnedSlice();
}

fn filter(self: *Self, input: []const u8) !void {
    const z = trace.zone(@src());
    defer z.end();

    //
    // @SEARCH: PICKALG
    //
    // Filter using a simple algorithm that is the same as the one
    // in Helix. For each item in the set, take the input filter and
    // select only the items that contain the input sequence in
    // order, even if there are multiple characters between the members
    // of the input sequence.
    //
    // For instance, given the set:
    // - dog
    // - dag
    // - da
    // - day
    // - cog
    // And the input filter "dg", the filtered set contains:
    // - dog
    // - dg
    //
    // It would be a good improvement to prefer longest continuous
    // matches over matches that are separated by some space. i.e.:
    // - src/maaaaain.zig
    // - src/main.zig
    // With the input filter "main.zig" should prefer the second one.
    //
    // If the filter text is "", then we select all entries in the set.
    //
    // We also sort the result according to the length of the string with
    // the assumption that the user typically accesses files that are
    // higher in the file system hierarchy.
    //

    self.selected_files.clearAndFree();

    if (input.len == 0) {
        for (self.file_paths.items, 0..) |_, ndx| {
            try self.selected_files.append(ndx);
        }
    } else {
        for (self.file_paths.items, 0..) |path, pathNdx| {
            var containsAll = false;
            var inputNdx: usize = 0;

            for (path) |ch| {
                const in = ascii.toLower(input[inputNdx]);
                if (ascii.toLower(ch) == in) {
                    inputNdx += 1;
                    if (inputNdx >= input.len) {
                        containsAll = true;
                        break;
                    }
                }
            }

            if (containsAll) {
                try self.selected_files.append(pathNdx);
            }
        }
    }

    mem.sort(usize, self.selected_files.items, self, compareSortOrder);
}

fn compareSortOrder(self: *Self, i: usize, j: usize) bool {
    const a = self.file_paths.items[i];
    const b = self.file_paths.items[j];
    return a.len < b.len;
}

/// caller does not own returned memory; it will be free'd by the
/// FilePicker itself
fn getPreview(self: *Self, selectedNdx: usize) ![]const u8 {
    const z = trace.zone(@src());
    defer z.end();

    if (self.selected_files.items.len == 0) return "";

    const fileNdx = self.selected_files.items[selectedNdx];
    const filePath = self.file_paths.items[fileNdx];
    return self.file_previews.items[fileNdx].getPreview(self.state, filePath);
}

pub fn view(self: *Self) State.View {
    const z = trace.zone(@src());
    defer z.end();

    return State.View{ .file_picker = self };
}

fn handleInput(self: *Self) ?State.View {
    const z = trace.zone(@src());
    defer z.end();

    if (Input.cancelPressed()) {
        self.hide();
        return self.state.primary.view();
    }

    if (Input.keyPressed(.up)) {
        self.moveCursor(.up);
    }
    if (Input.keyPressed(.down)) {
        // @TODO (jrc): handle scrolling off the bottom
        self.moveCursor(.down);
    }

    if (Input.keyPressedWithCtrl(.n)) {
        self.moveCursor(.down);
    }
    if (Input.keyPressedWithCtrl(.p)) {
        self.moveCursor(.up);
    }

    if (Input.mouse_was_moved) {
        self.hide_mouse = false;
    }

    return null;
}

pub fn update(self: *Self) State.View {
    const z = trace.zoneN(@src(), "FilePicker.update");
    defer z.end();

    if (self.handleInput()) |next| return next;
    var next = self.view();

    const window_size = self.gui.getSingleFocusWindowSize();
    const half_width = @divFloor(window_size.w, 2);

    zui.setNextWindowPos(window_size.x, window_size.y, 0, 0);
    zui.setNextWindowSize(window_size.w, window_size.h);
    zui.setNextWindowFocus();

    if (zui.begin(windows.FilePicker_List, .{
        .flags = .{
            .no_resize = true,
            .no_title_bar = true,
            .no_move = true,
        },
    })) {
        defer zui.end();

        if (zui.beginChild("File Chooser", .{
            .w = half_width,
            .flags = .{
                .no_title_bar = true,
            },
        })) {
            defer zui.endChild();

            //
            // File Picker Text Input
            //

            zui.setKeyboardFocusHere(0);

            zui.pushItemWidth(-1);
            var buf = [_]u8{0} ** 256;
            if (zui.inputTextWithHint("search", .{
                .flags = .{
                    .enter_returns_true = true,
                    .callback_edit = true,
                },
                .hint = "search for a file, using ctrl+n and ctrl+p to navigate",
                .buf = &buf,
                .callback = &textChanged,
                .user_data = self,
            })) {
                if (self.selected_files.items.len > 0) {
                    const ndx = self.cursor_ndx;
                    const fileNdx = self.selected_files.items[ndx];
                    const selected = self.file_paths.items[fileNdx];

                    self.state.openSourceFile(selected) catch |err| {
                        log.errf("unable to open source file: {!}", .{err});
                        return self.state.primary.view();
                    };

                    self.hide();
                    next = self.state.primary.view();
                }
            }
            zui.popItemWidth();

            //
            // File Picker List
            //

            if (zui.beginTable("File Picker List", .{
                .flags = .{ .scroll_y = true },
                .column = 1,
            })) {
                defer zui.endTable();

                const filteredFiles = self.getFilteredPaths() catch |err| {
                    log.errf("unable to filter files: {!}", .{err});
                    return self.state.primary.view();
                };
                defer self.state.perm_alloc.free(filteredFiles);

                // ensure no index out of bounds
                if (filteredFiles.len <= 0) {
                    self.cursor_ndx = 0;
                } else if (self.cursor_ndx >= filteredFiles.len) {
                    self.cursor_ndx = filteredFiles.len - 1;
                }

                for (filteredFiles, 0..) |fpath, ndx| {
                    zui.tableNextRow(.{});

                    if (zui.tableNextColumn()) {
                        // the default background if a user has
                        // not arrowed up/down
                        var bg = [4]f32{ 0, 0, 0, 1 };
                        if (ndx % 2 == 0) {
                            bg = .{ 0.1, 0.1, 0.1, 1 };
                        }

                        if (self.hide_mouse) {
                            // using the keyboard to choose
                            const cursor = self.cursor_ndx;

                            if (cursor == ndx) {
                                // set to the same style as mouse hover
                                const style = zui.getStyle();
                                bg = style.getColor(.header_hovered);
                            }

                            zui.tableSetBgColor(.{
                                .target = .cell_bg,
                                .color = zui.colorConvertFloat4ToU32(bg),
                            });

                            zui.textWrapped("{s}", .{fpath});
                        } else {
                            // using the mouse to choose
                            zui.tableSetBgColor(.{
                                .target = .cell_bg,
                                .color = zui.colorConvertFloat4ToU32(bg),
                            });

                            // zero terminate the string
                            var name = [_]u8{0} ** 256;
                            mem.copyForwards(u8, &name, fpath);

                            _ = zui.selectable(@ptrCast(&name), .{ .flags = .{
                                .span_all_columns = true,
                            } });

                            if (zui.isItemHovered(.{})) {
                                self.cursor_ndx = ndx;
                                if (zui.isMouseClicked(.left)) {
                                    self.state.openSourceFile(fpath) catch |err| {
                                        log.errf("unable to open source file: {!}", .{err});
                                        return self.state.primary.view();
                                    };
                                    self.hide();
                                    next = self.state.primary.view();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        zui.sameLine(.{});
        if (zui.beginChild("File Preview", .{
            .w = half_width,
            .flags = .{
                .no_title_bar = true,
            },
        })) {
            defer zui.endChild();

            const preview = self.getPreview(self.cursor_ndx) catch |err| {
                log.errf("unable to get file preview: {!}", .{err});
                return self.state.primary.view();
            };
            zui.textWrapped("{s}", .{preview});
        }
    }

    return next;
}

fn hide(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.hide_mouse = true;
    self.cursor_ndx = 0;
    self.reset() catch |err| {
        log.errf("unable to hide file picker: {!}", .{err});
        return;
    };
}

fn textChanged(args: [*c]zui.InputTextCallbackData) callconv(.C) i32 {
    const z = trace.zone(@src());
    defer z.end();

    assert(args.*.user_data != null);
    const self: *Self = @ptrCast(@alignCast(args.*.user_data));

    const buf = args.*.buf[0..@intCast(args.*.buf_text_len)];
    self.filter(buf) catch |err| {
        log.errf("unable to filter files: {!}", .{err});
        return 0;
    };

    return 0;
}

const CursorDirection = enum(u8) {
    up,
    down,
};

fn moveCursor(self: *Self, dir: CursorDirection) void {
    switch (dir) {
        .up => {
            if (self.cursor_ndx > 0) {
                self.cursor_ndx -= 1;
            }
        },
        .down => {
            // @NOTE (jrc): bounds checking on this ndx will be performed
            // once we have the actual list of files later on in the frame
            self.cursor_ndx += 1;
        },
    }

    self.hide_mouse = true;
}
