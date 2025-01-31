const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const fmt = std.fmt;
const io = std.io;
const mem = std.mem;
const posix = std.posix;
const testing = std.testing;

const logging = @import("logging.zig");

const file = @import("file.zig");
const trace = @import("trace.zig");

/// @NOTE (jrc): settings.parseFiles is called before Logger.init, so
/// we're unable to log from that code path
const log = logging.Logger.init(logging.Region.Settings);

/// Global variable that contains all the user's settings. It may be read
/// from any thread, and may never be modified after startup.
pub var settings = Settings{};

const Settings = struct {
    /// Settings pertaining to all projects on the system
    global: Global = Global{},

    /// Settings pertaining to only the current project
    project: Project = Project{},
};

const Global = struct {
    log: Log = Log{},
    display: Display = Display{},
    rust: Rust = Rust{},

    fn mapEntry(global: *@This(), allocator: Allocator, entry: *const IniEntry) !void {
        if (mem.eql(u8, entry.section, "log")) {
            try global.log.mapEntry(allocator, entry);
            return;
        }
        if (mem.eql(u8, entry.section, "rust")) {
            try global.rust.mapEntry(allocator, entry);
            return;
        }
    }
};

const Log = struct {
    /// Whether or not to enable color in log output
    color: bool = true,

    /// Indicates the minimum severity level that
    /// will be output in the log file
    level: logging.Level = .err,

    /// CSV of the log regions to turn on (or "all" to enable
    /// all log regions)
    regions: []const u8 = "none",

    /// The absolute path to the file where logs will be written
    file: []const u8 = "/tmp/uscope.log",

    fn mapEntry(self: *@This(), allocator: Allocator, entry: *const IniEntry) !void {
        if (mem.eql(u8, entry.key, "color")) {
            self.color = try parseBool(entry.val);
            return;
        }
        if (mem.eql(u8, entry.key, "level")) {
            self.level = try logLevelFromStr(allocator, entry.val);
            return;
        }
        if (mem.eql(u8, entry.key, "regions")) {
            self.regions = try allocString(allocator, entry.val);
            return;
        }
        if (mem.eql(u8, entry.key, "file")) {
            self.file = try allocString(allocator, entry.val);
            return;
        }
    }
};

/// Allocates only temporarily
fn logLevelFromStr(alloc: Allocator, level: []const u8) !logging.Level {
    const lvl = try alloc.alloc(u8, level.len);
    defer alloc.free(lvl);

    for (level, 0..) |c, ndx| lvl[ndx] = std.ascii.toLower(c);

    return logging.Level.fromStr(lvl);
}

const Display = struct {
    /// How many lines of program output to retain on each run (larger values use more memory)
    output_bytes: usize = 1024 * 8,
};

/// Rust builds to special paths on the user's system, so progammers using rust need
/// to supply a couple additional paths so we can load their debug symbols
const Rust = struct {
    /// The path to the rust stdlib
    /// (i.e. /home/user/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/)
    stdlib: []const u8 = "",

    /// The path to cargo packages
    /// (i.e. /home/user/.cargo/registry/src/)
    cargo: []const u8 = "",

    fn mapEntry(self: *@This(), allocator: Allocator, entry: *const IniEntry) !void {
        if (mem.eql(u8, entry.key, "stdlib")) {
            self.stdlib = try allocString(allocator, mem.trimRight(u8, entry.val, "/"));
            return;
        }
        if (mem.eql(u8, entry.key, "cargo")) {
            self.cargo = try allocString(allocator, mem.trimRight(u8, entry.val, "/"));
            return;
        }
    }
};

const Project = struct {
    sources: Sources = Sources{},

    target: Target = Target{},

    fn mapEntry(project: *@This(), allocator: Allocator, entry: *const IniEntry) !void {
        if (mem.eql(u8, entry.section, "sources")) {
            try project.sources.mapEntry(allocator, entry);
            return;
        }
        if (mem.eql(u8, entry.section, "target")) {
            try project.target.mapEntry(allocator, entry);
            return;
        }
    }
};

const Sources = struct {
    /// CSV of a path to a file the user wishes to automatically open, followed by a
    /// colon-delimited of the lines on which breakpoints should be set in that file.
    /// For example:
    ///
    /// src/main.c:5:6:7,src/foo.c:10
    open_files: [][]const u8 = &.{},

    /// Not a field that is parsed from the settings file, but is automatically populated
    /// based on the contents of `open_files`. Each index in this array corresponds to a
    /// matching entry in `open_files`.
    breakpoint_lines: [][]usize = &.{},

    fn mapEntry(self: *@This(), allocator: Allocator, entry: *const IniEntry) !void {
        if (mem.eql(u8, entry.key, "open_files")) {
            const entries = try allocStringSlice(allocator, entry.val);
            errdefer allocator.free(entries);

            var open_files = try ArrayList([]const u8).initCapacity(allocator, entries.len);
            errdefer open_files.deinit();

            var breakpoint_lines = try ArrayList([]usize).initCapacity(allocator, entries.len);
            errdefer breakpoint_lines.deinit();

            // parse each breakpoint to be set from each file
            for (entries) |full_entry| {
                if (mem.indexOfScalar(u8, full_entry, ':')) |colon_ndx| {
                    open_files.appendAssumeCapacity(full_entry[0..colon_ndx]);

                    var bp_lines = ArrayList(usize).init(allocator);
                    errdefer bp_lines.deinit();

                    var it = mem.splitScalar(u8, full_entry[colon_ndx + 1 ..], ':');
                    while (it.next()) |part| {
                        const line = try fmt.parseInt(usize, part, 10);
                        try bp_lines.append(line);
                    }

                    breakpoint_lines.appendAssumeCapacity(try bp_lines.toOwnedSlice());
                } else {
                    // no breakpoints to set in this file
                    open_files.appendAssumeCapacity(full_entry);
                    breakpoint_lines.appendAssumeCapacity(&.{});
                    continue;
                }
            }

            self.open_files = try open_files.toOwnedSlice();
            self.breakpoint_lines = try breakpoint_lines.toOwnedSlice();

            return;
        }
    }
};

const Target = struct {
    /// The path to the executable the user wishes to debug
    path: []const u8 = "",

    /// The arguments to pass to the debugee as a CSV
    args: []const u8 = "",

    /// Whether or not to pause the subordinate when it is launched
    stop_on_entry: bool = false,

    /// The default set of expressions to use in the watch window
    watch_expressions: [][]const u8 = &.{},

    fn mapEntry(self: *@This(), allocator: Allocator, entry: *const IniEntry) !void {
        if (mem.eql(u8, entry.key, "path")) {
            self.path = try allocString(allocator, entry.val);
            return;
        }
        if (mem.eql(u8, entry.key, "args")) {
            self.args = try allocString(allocator, entry.val);
            return;
        }
        if (mem.eql(u8, entry.key, "stop_on_entry")) {
            self.stop_on_entry = try parseBool(entry.val);
            return;
        }
        if (mem.eql(u8, entry.key, "watch_expressions")) {
            self.watch_expressions = try allocStringSlice(allocator, entry.val);
            return;
        }
    }
};

const IniEntry = struct {
    section: []const u8,
    key: []const u8,
    val: []const u8,
};

/// Caller owns returned memory
pub fn globalConfigDir(allocator: Allocator) !ArrayList(u8) {
    var dir = ArrayList(u8).init(allocator);

    // XDG_CONFIG_HOME specification: https://specifications.freedesktop.org/basedir-spec/latest/
    if (posix.getenv("XDG_CONFIG_HOME")) |xdg_config_home| {
        try dir.appendSlice(xdg_config_home);
    } else if (posix.getenv("HOME")) |home| {
        try dir.appendSlice(home);
        try dir.appendSlice("/.config");
    } else {
        const w = io.getStdErr().writer();
        try w.print("unable to get XDG_CONFIG_HOME environment variable\n", .{});
        return error.InvalidGlobalConfigPath;
    }

    if (!mem.endsWith(u8, dir.items, "/")) try dir.append('/');

    return dir;
}

pub fn parseFiles(allocator: Allocator) !void {
    const z = trace.zone(@src());
    defer z.end();

    //
    // @TODO (jrc): if either the local or global settings files don't
    // exist, we should not error, but instead set reasonable defaults
    // and perhaps create the file on the user's behalf
    //

    const project_path = ".uscope/config.ini";
    var project_fp = file.open(project_path, .{ .mode = .read_only }) catch |err| {
        std.debug.print("unable to open project settings file \"{}\": {!}\n", .{ std.zig.fmtEscapes(project_path), err });
        return err;
    };
    defer project_fp.close();

    const projectContents = file.mapWholeFile(project_fp) catch |err| {
        std.debug.print("unable to open project settings file \"{}\": {!}\n", .{ std.zig.fmtEscapes(project_path), err });
        return err;
    };
    defer file.munmap(projectContents);

    var global_path = try globalConfigDir(allocator);
    defer global_path.deinit();

    // create the global config file if it doesn't already exist
    const global_cfg_name = "uscope";
    const global_dir = try std.fs.openDirAbsolute(global_path.items, .{});
    global_dir.access(global_cfg_name, .{}) catch |err| switch (err) {
        error.FileNotFound => try global_dir.makeDir(global_cfg_name),
        else => return err,
    };

    try global_path.appendSlice(global_cfg_name ++ "/config.ini");
    std.fs.accessAbsolute(global_path.items, .{}) catch |err| switch (err) {
        error.FileNotFound => {
            // create an one-byte file
            const global_create = try std.fs.createFileAbsolute(global_path.items, .{ .truncate = false });
            defer global_create.close();
            try global_create.writeAll("\n");
        },
        else => return err,
    };

    var global_fp = file.open(global_path.items, .{ .mode = .read_only }) catch |err| {
        std.debug.print("unable to open global settings file: {!}\n", .{err});
        return err;
    };
    defer global_fp.close();

    const globalContents = try file.mapWholeFile(global_fp);
    defer file.munmap(globalContents);

    try parseAll(allocator, &settings, globalContents, projectContents);
}

fn parseAll(allocator: Allocator, dest: *Settings, globalContents: []const u8, projectContents: []const u8) !void {
    const z = trace.zone(@src());
    defer z.end();

    try parseOne(Global, allocator, &dest.global, globalContents);
    try parseOne(Project, allocator, &dest.project, projectContents);
}

fn parseOne(comptime T: anytype, allocator: Allocator, dest: *T, contents: []const u8) !void {
    const z = trace.zone(@src());
    defer z.end();

    var fbs = std.io.fixedBufferStream(contents);
    const reader = fbs.reader();

    var section = ArrayList(u8).init(allocator);
    defer section.deinit();

    var line_buf = ArrayList(u8).init(allocator);
    defer line_buf.deinit();

    const whitespace = " \r\t\x00";

    // @ROBUSTNESS (jrc): no infinite loops
    while (true) {
        // read a full line
        var done = false;
        reader.readUntilDelimiterArrayList(&line_buf, '\n', 256) catch |err| switch (err) {
            error.EndOfStream => {
                if (line_buf.items.len == 0) {
                    done = true;
                }
            },
            else => return err,
        };
        if (done) {
            break;
        }

        // append sentinel
        try line_buf.append(0);

        const line = blk: {
            // comment or blank lines
            var l = mem.trim(u8, line_buf.items, whitespace);
            if (l.len == 0 or l[0] == '#' or l[0] == ';') continue;

            // remove trailing comments
            if (mem.indexOfScalar(u8, l, '#')) |ndx| {
                assert(ndx > 0);
                l = l[0 .. ndx - 1];
            }

            break :blk l;
        };

        if (mem.startsWith(u8, line, "[") and mem.endsWith(u8, line, "]")) {
            // we've read a new section header
            const name = mem.trim(u8, line, "[]");
            section.clearAndFree();
            for (name) |c| try section.append(c);
            continue;
        }

        const ndx = mem.indexOfScalar(u8, line, '=');
        if (ndx == null) {
            log.warnf("malformed setting (missing \"=\"): {s}", .{line});
            continue;
        }

        // trim leading and trailing whitespace and quotes
        const key = mem.trim(u8, line[0..ndx.?], whitespace ++ "'\"");
        const val = mem.trim(u8, line[ndx.? + 1 ..], whitespace ++ "'\"");

        var sectionLowerBuf: [256]u8 = undefined;
        var keyLowerBuf: [256]u8 = undefined;

        const entry = IniEntry{
            .section = std.ascii.lowerString(&sectionLowerBuf, section.items),
            .key = std.ascii.lowerString(&keyLowerBuf, key),
            .val = val,
        };

        try dest.mapEntry(allocator, &entry);
    }
}

fn parseBool(val: []const u8) !bool {
    if (mem.eql(u8, val, "true")) {
        return true;
    }
    if (mem.eql(u8, val, "false")) {
        return false;
    }

    return error.InvalidBoolVal;
}

/// Saves the string to a permanent arena for later use
fn allocString(allocator: Allocator, val: []const u8) ![]const u8 {
    const whitespace = " ";
    var str = mem.trimLeft(u8, val, whitespace);
    str = mem.trimRight(u8, str, whitespace);

    const copy = try allocator.alloc(u8, str.len);
    @memcpy(copy, str);
    return copy;
}

/// Saves the array of strings (separated by a comma) to a permanent arena for later use
fn allocStringSlice(allocator: Allocator, val: []const u8) ![][]const u8 {
    var arr = ArrayList([]const u8).init(allocator);
    errdefer arr.deinit();

    var it = mem.splitSequence(u8, val, ",");
    while (it.next()) |part| {
        const str = try allocString(allocator, part);
        try arr.append(str);
    }

    return arr.toOwnedSlice();
}

/// Parses the array of numbers from strings to a permanent arena for later use
fn allocNumericSlice(comptime T: type, allocator: Allocator, val: []const u8) ![]T {
    var arr = ArrayList(T).init(allocator);
    errdefer arr.deinit();

    var it = mem.splitSequence(u8, val, ",");
    while (it.next()) |part| {
        const n = try fmt.parseInt(T, part, 10);
        try arr.append(n);
    }

    return arr.toOwnedSlice();
}

test "settings ini correctly parses" {
    var arena = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena.deinit();
    const allocator = arena.allocator();

    {
        // test default values
        const globalContents = "";

        var g = Global{};
        try parseOne(Global, allocator, &g, globalContents);

        try testing.expect(g.log.color);
        try testing.expectEqual(logging.Level.err, g.log.level);
        try testing.expectEqualStrings("none", g.log.regions);
        try testing.expectEqualStrings("/tmp/uscope.log", g.log.file);
    }

    {
        // test custom values
        const globalContents =
            \\ [log]
            \\ color = false
            \\ level =  'warn'
            \\ regions = "main,dwarf,other"
            \\ file = /tmp/testing.123
        ;

        var g = Global{};
        try parseOne(Global, allocator, &g, globalContents);

        try testing.expect(!g.log.color);
        try testing.expectEqual(logging.Level.wrn, g.log.level);
        try testing.expectEqualStrings("main,dwarf,other", g.log.regions);
        try testing.expectEqualStrings("/tmp/testing.123", g.log.file);
    }

    {
        // test default values
        const projectContents = "";

        var p = Project{};
        try parseOne(Project, allocator, &p, projectContents);

        try testing.expectEqualStrings("", p.target.path);
        try testing.expectEqualStrings("", p.target.args);
    }

    {
        // test custom values
        const projectContents =
            \\ [target]
            \\ path=assets/cloop/loop
            \\
            \\ ;args=foo
            \\ #args=bar
            \\     #  args=baz
            \\ args= one, two, three # trailing comment
        ;

        var p = Project{};
        try parseOne(Project, allocator, &p, projectContents);

        try testing.expectEqualStrings("assets/cloop/loop", p.target.path);
        try testing.expectEqualStrings("one, two, three", p.target.args);
    }
}
