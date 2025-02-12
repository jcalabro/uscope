const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const fmt = std.fmt;
const fs = std.fs;
const mem = std.mem;
const Mutex = std.Thread.Mutex;
const time = std.time;
const t = std.testing;

const BreakpointView = @import("views/Breakpoint.zig");
const CircularBuffer = @import("../circularBuffer.zig").CircularBuffer;
const debugger = @import("../debugger.zig");
const Debugger = debugger.Debugger;
const FilePickerView = @import("views/FilePicker.zig");
const file_util = @import("../file.zig");
const Input = @import("Input.zig");
const logging = @import("../logging.zig");
const PrimaryView = @import("views/Primary.zig");
const proto = debugger.proto;
const settings = @import("../settings.zig");
const strings = @import("../strings.zig");
const String = strings.String;
const trace = @import("../trace.zig");
const types = @import("../types.zig");
const Watcher = @import("watcher.zig").Watcher;

const Self = @This();

pub const GUIType = switch (builtin.is_test) {
    true => @import("../test/simulator.zig").TestGUI,
    false => @import("GUI.zig"),
};

const log = logging.Logger.init(logging.Region.GUI);

perm_alloc: Allocator,

/// reset each time we get an update from the Debugger
scratch_arena: ArenaAllocator,
scratch_alloc: Allocator = undefined,
state_updated: bool = true,

first_frame: bool = true,
shutting_down: bool = false,

dbg: *Debugger,
dbg_state: types.StateSnapshot = mem.zeroes(types.StateSnapshot),

watcher: *Watcher = undefined,

subordinate_output: CircularBuffer(u8),
subordinate_output_mu: Mutex = Mutex{},

active_view: View = undefined,

primary: *PrimaryView = undefined,
file_picker: *FilePickerView = undefined,
breakpoint: *BreakpointView = undefined,

open_files: ArrayList(OpenFile),
open_source_file_ndx: usize = 0,

scroll_to_line_of_text: ?types.SourceLocation = null,
/// @NOTE (jrc): I'm not sure why, but the imgui flag to
/// auto-open newly opened tabs isn't working, so we do
/// it manually using this field
newly_opened_file: ?usize = null,
/// @NOTE (jrc): for the same reason as newly_opened_file, we
/// need to wait one frame for the file to be open and rendered
/// before we can actually perform scrolling on stepping in to
/// a file that was not already open. :headdesk:
has_waited_one_frame_to_scroll_to_line_of_text: bool = true,

pub const View = union(enum) {
    primary: *PrimaryView,
    file_picker: *FilePickerView,
    breakpoint: *BreakpointView,
};

pub fn init(alloc: Allocator, dbg: *Debugger, gui: *GUIType) !*Self {
    const z = trace.zoneN(@src(), "State.init");
    defer z.end();

    Input.init(alloc);

    const self = try alloc.create(Self);
    errdefer alloc.destroy(self);

    self.* = .{
        .perm_alloc = alloc,
        .scratch_arena = ArenaAllocator.init(alloc),
        .dbg = dbg,
        .open_files = ArrayList(OpenFile).init(alloc),
        .subordinate_output = try CircularBuffer(u8).init(
            self.perm_alloc,
            settings.settings.global.display.output_bytes,
        ),
        .watcher = Watcher.init(
            alloc,
            settings.settings.project.target.path,
            self,
            executableFileChanged,
        ) catch |err| {
            const msg = try fmt.allocPrint(alloc, "unable to open target executable \"{}\": {!}", .{
                std.zig.fmtEscapes(settings.settings.project.target.path),
                err,
            });
            defer alloc.free(msg);

            log.errf("{s}", .{msg});
            std.debug.print("{s}\n", .{msg});
            return err;
        },
    };

    self.scratch_alloc = self.scratch_arena.allocator();

    self.primary = try PrimaryView.init(self, gui);
    self.file_picker = try FilePickerView.init(self, gui);
    self.breakpoint = try BreakpointView.init(self, gui);

    self.active_view = View{ .primary = self.primary };

    return self;
}

pub fn deinit(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.watcher.deinit(self.perm_alloc);

    self.scratch_arena.deinit();

    self.open_files.deinit();
    self.subordinate_output.deinit(self.perm_alloc);
    self.perm_alloc.destroy(self);
}

pub fn quit(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.shutting_down = true;
    self.dbg.enqueue(proto.QuitRequest{});
}

pub fn update(self: *Self) void {
    const z = trace.zoneN(@src(), "State.update");
    defer z.end();

    Input.calculateMouseMovement();

    switch (self.active_view) {
        inline else => |view| {
            if (self.state_updated) {
                self.state_updated = false;
                _ = self.scratch_arena.reset(.free_all);

                if (self.getStateSnapshot(self.scratch_alloc)) |s| {
                    self.updateSourceLocationInFocus(s.state);
                    self.dbg_state = s.state;
                } else |err| {
                    log.errf("error getting frame state: {!}", .{err});
                    return;
                }
            }

            self.handleDebuggerResponses();
            self.active_view = view.update();
        },
    }

    self.first_frame = false;

    if (builtin.mode == .Debug) log.flush();
}

pub fn getStateSnapshot(self: *Self, alloc: Allocator) !proto.GetStateResponse {
    return self.dbg.handleRequest(
        proto.GetStateResponse,
        proto.GetStateRequest{ .alloc = alloc },
    );
}

fn handleDebuggerResponses(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    // only allow so many messages per tick
    const max = 512;
    for (0..max) |_| {
        const resp = self.dbg.responses.getOrNull();
        if (resp == null) break;

        switch (resp.?) {
            .reset => {
                self.subordinate_output_mu.lock();
                defer self.subordinate_output_mu.unlock();

                self.subordinate_output.clearAndReset(self.perm_alloc) catch |err| {
                    log.errf("unable to reset subordinate output buffer: {!}", .{err});
                };
            },

            .received_text_output => |r| {
                defer self.dbg.responses.alloc.free(r.text);

                self.subordinate_output_mu.lock();
                defer self.subordinate_output_mu.unlock();

                // @PERFORMANCE (jrc): appendRange rather than a for loop
                for (r.text) |byte| self.subordinate_output.append(byte);
            },

            .load_symbols => {
                for (settings.settings.project.sources.open_files, 0..) |file_path, ndx| {
                    const abs_path = fs.realpathAlloc(self.scratch_alloc, file_path) catch |err| {
                        log.errf("unable to get realpath for {s}: {!}", .{ file_path, err });
                        continue;
                    };

                    for (settings.settings.project.sources.breakpoint_lines[ndx]) |line| {
                        self.updateBreakpoint(abs_path, types.SourceLine.from(line));
                    }
                }
            },

            .state_updated => self.state_updated = true,

            inline else => |cmd| {
                log.warnf("unhandled response: {any}", .{cmd});
            },
        }
    }
}

pub fn loadDebugSymbols(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.dbg.enqueue(proto.LoadSymbolsRequest{
        .path = settings.settings.project.target.path,
    });
}

/// This function is called back whenever the file on disk is modified
fn executableFileChanged(self: *Self) void {
    self.loadDebugSymbols();

    // @TODO (jrc): adjust breakpoint line numbers
}

pub fn launchSubordinate(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.dbg.enqueue(proto.LaunchSubordinateRequest{
        .args = settings.settings.project.target.args,
        .path = settings.settings.project.target.path,
        .stop_on_entry = settings.settings.project.target.stop_on_entry,
    });
}

pub fn killSubordinate(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.dbg.enqueue(proto.KillSubordinateRequest{});
}

pub fn continueExecution(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    self.dbg.enqueue(proto.ContinueRequest{});
}

pub fn updateBreakpoint(self: *Self, abs_file_path: String, line: types.SourceLine) void {
    assert(fs.path.isAbsolute(abs_file_path));
    self.dbg.enqueue(proto.UpdateBreakpointRequest{ .loc = .{
        .source = .{
            .file_hash = file_util.hashAbsPath(abs_file_path),
            .line = line,
        },
    } });
}

pub const OpenFile = struct {
    open: bool = true,

    abs_path_hash: u64,
    abs_path: String,
    rel_path: String,
    name: String,
    lines: []String,

    language: types.Language,

    pub fn deinit(self: @This(), alloc: Allocator) void {
        alloc.free(self.rel_path);
        alloc.free(self.name);

        for (self.lines) |line| alloc.free(line);
        alloc.free(self.lines);
    }
};

/// Loads a source file from disk for display in the primary source viewer
pub fn openSourceFile(self: *Self, relative_path: String) !void {
    const z = trace.zone(@src());
    defer z.end();

    for (self.open_files.items, 0..) |open, ndx| {
        if (mem.eql(u8, open.rel_path, relative_path)) {
            // file is already open
            self.newly_opened_file = ndx;
            return;
        }
    }

    log.debugf("opening file: {s}", .{relative_path});

    const p = try self.perm_alloc.alloc(u8, relative_path.len);
    errdefer self.perm_alloc.free(p);
    @memcpy(p, relative_path);

    const cwd = fs.cwd();
    const fp = try cwd.openFile(relative_path, .{ .mode = .read_only });
    defer fp.close();

    const abs_path = try cwd.realpathAlloc(self.perm_alloc, relative_path);
    errdefer self.perm_alloc.free(abs_path);

    const contents = try file_util.mapWholeFile(fp);
    defer file_util.munmap(contents);

    const lines = blk: {
        var arr = ArrayList(String).init(self.perm_alloc);
        errdefer {
            for (arr.items) |l| self.perm_alloc.free(l);
            arr.deinit();
        }

        var it = mem.splitSequence(u8, contents, file_util.LineDelimiter);
        while (it.next()) |line| {
            const copy = try self.perm_alloc.alloc(u8, line.len);
            errdefer self.perm_alloc.free(line);
            @memcpy(copy, line);

            try arr.append(copy);
        }

        break :blk try arr.toOwnedSlice();
    };
    errdefer {
        for (lines) |l| self.perm_alloc.free(l);
        self.perm_alloc.free(lines);
    }

    try self.open_files.append(OpenFile{
        .abs_path_hash = file_util.hashAbsPath(abs_path),
        .abs_path = abs_path,
        .rel_path = p,
        .name = fs.path.basename(p),
        .lines = lines,
        .language = types.Language.fromPath(p),
    });

    self.newly_opened_file = self.open_files.items.len - 1;
}

/// closes the file at the given ndx. If the ndx is null, it defaults to closing
/// the file that is currently open in the viewer
pub fn closeSourceFile(self: *Self, file_ndx: ?usize) void {
    const z = trace.zone(@src());
    defer z.end();

    if (self.open_files.items.len == 0) return;

    var ndx = self.open_source_file_ndx;
    if (file_ndx) |i| ndx = i;

    const f = self.open_files.orderedRemove(ndx);
    f.deinit(self.perm_alloc);
}

test "load source files for display" {
    var arena = ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const alloc = arena.allocator();

    const self = try alloc.create(Self);
    self.* = .{
        .perm_alloc = alloc,
        .scratch_arena = ArenaAllocator.init(alloc),
        .dbg = undefined,
        .open_files = ArrayList(OpenFile).init(alloc),
        .subordinate_output = try CircularBuffer(u8).init(alloc, 1024),
    };
    self.scratch_alloc = self.scratch_arena.allocator();

    try t.expectEqual(@as(usize, 0), self.open_files.items.len);
    try self.openSourceFile("assets/cloop/main.c");
    try t.expectEqual(@as(usize, 1), self.open_files.items.len);

    const f = self.open_files.items[0];
    try t.expectEqual(@as(usize, 16), f.lines.len);
    try t.expectEqualStrings("assets/cloop/main.c", f.rel_path);
    try t.expectEqualStrings("main.c", f.name);
    try t.expectEqual(types.Language.C, f.language);
}

pub fn sendStepRequest(self: *Self, step_type: proto.StepType) void {
    const z = trace.zone(@src());
    defer z.end();

    self.dbg.enqueue(proto.StepRequest{ .step_type = step_type });
}

fn updateSourceLocationInFocus(self: *Self, new_state: types.StateSnapshot) void {
    const z = trace.zone(@src());
    defer z.end();

    if (new_state.paused == null or new_state.paused.?.source_location == null) return;
    const new_loc = new_state.paused.?.source_location.?;

    if (self.dbg_state.paused) |paused| {
        if (paused.source_location) |existing_loc| {
            // we got a new state snapshot but the source line in focus did not
            // change; nothing to do
            if (existing_loc.eql(new_loc)) return;
        }
    }

    const already_open: ?usize = blk: {
        for (self.open_files.items, 0..) |f, ndx| {
            if (f.open and f.abs_path_hash == new_loc.file_hash) break :blk ndx;
        }
        break :blk null;
    };

    if (already_open) |ndx| {
        self.open_source_file_ndx = ndx;
        self.newly_opened_file = ndx;
    } else {
        if (file_util.getCachedFile(new_loc.file_hash)) |f| {
            self.openSourceFile(f.abs_path) catch |err| {
                log.errf("unable to open source file {s}: {!}", .{ f.abs_path, err });
                return;
            };
            self.open_source_file_ndx = self.open_files.items.len - 1;
        } else {
            log.errf("unable to find file name for hash: 0x{x}", .{new_loc.file_hash});
            return;
        }
    }

    self.scroll_to_line_of_text = new_loc;
    self.has_waited_one_frame_to_scroll_to_line_of_text = false;
}
