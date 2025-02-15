const std = @import("std");
const Allocator = mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const fs = std.fs;
const math = std.math;
const mem = std.mem;
const Mutex = Thread.Mutex;
const pow = std.math.pow;
const t = std.testing;
const Thread = std.Thread;
const ThreadSafeAllocator = std.heap.ThreadSafeAllocator;
const time = std.time;

const debugger = @import("../debugger.zig");
const Debugger = debugger.Debugger;
const file_utils = @import("../file.zig");
const flags = @import("../flags.zig");
const GUI = @import("../gui/GUI.zig");
const logging = @import("../logging.zig");
const proto = debugger.proto;
const State = @import("../gui/State.zig");
const strings = @import("../strings.zig");
const String = strings.String;
const types = @import("../types.zig");
const zui = @import("../gui/zui.zig");

const log = logging.Logger.init(logging.Region.Test);

/// A Mock GUI implementation for the few cross-cutting concerns that
/// haven't (yet) been eliminated
pub const TestGUI = struct {
    const Self = @This();

    pub fn getMainDockspaceID(_: Self) zui.ID {
        return 0;
    }

    pub fn getSingleFocusWindowSize(self: Self) GUI.WindowSize {
        return self.getSingleFocusWindowSizeWithScale(0);
    }

    pub fn getSingleFocusWindowSizeWithScale(_: Self, _: f32) GUI.WindowSize {
        return .{ .x = 0, .y = 0, .w = 800, .h = 600 };
    }
};

fn fileHash(alloc: Allocator, path: String) !file_utils.Hash {
    const cwd = fs.cwd();
    const abs_path = try cwd.realpathAlloc(alloc, path);
    defer t.allocator.free(abs_path);

    return file_utils.hashAbsPath(abs_path);
}

const Simulator = struct {
    const Self = @This();

    arena: *ArenaAllocator,
    thread_safe_alloc: *ThreadSafeAllocator,
    alloc: Allocator,

    dbg: *Debugger,
    gui: *TestGUI,
    state: *State,

    /// About 16 seconds
    max_iterations: usize = pow(usize, 2, 10),
    last_frame_render_micros: u64 = 0,

    event_count: usize = 0,
    last_event: usize = 0,
    tick_count: i32 = 0,
    last_event_tick: i32 = 0,

    mu: Mutex = .{},
    commands: ArrayList(Command),
    conditions: ArrayList(Condition),

    /// Disables a test
    skip: bool = false,

    fn init(root_alloc: Allocator) !*Self {
        const file_cache = try file_utils.Cache.init(root_alloc);
        errdefer file_cache.deinit();

        const thread_safe_alloc = try root_alloc.create(ThreadSafeAllocator);
        errdefer root_alloc.destroy(thread_safe_alloc);
        thread_safe_alloc.* = .{ .child_allocator = root_alloc };
        const alloc = thread_safe_alloc.allocator();

        var dbg = try Debugger.init(thread_safe_alloc, file_cache);
        errdefer dbg.deinit();

        const self = try alloc.create(Self);
        errdefer alloc.destroy(self);

        var arena = try alloc.create(ArenaAllocator);
        errdefer arena.deinit();
        arena.* = ArenaAllocator.init(alloc);
        const arena_alloc = arena.allocator();

        const gui = try arena_alloc.create(TestGUI);

        var state = try State.init(arena_alloc, dbg, gui);
        errdefer state.deinit();

        self.* = .{
            .arena = arena,
            .thread_safe_alloc = thread_safe_alloc,
            .alloc = alloc,

            .dbg = dbg,
            .gui = gui,
            .state = state,

            .commands = ArrayList(Command).init(alloc),
            .conditions = ArrayList(Condition).init(alloc),
        };

        return self;
    }

    fn deinit(self: *Self, root_alloc: Allocator) void {
        self.dbg.file_cache.deinit();
        self.dbg.deinit();

        self.commands.deinit();
        self.conditions.deinit();

        self.state.deinit();

        self.arena.deinit();
        self.alloc.destroy(self.arena);

        const thread_safe_alloc = self.thread_safe_alloc;
        self.alloc.destroy(self);
        root_alloc.destroy(thread_safe_alloc);

        log.flush();
    }

    fn lock(self: *Self) *Self {
        self.mu.lock();
        return self;
    }

    fn unlock(self: *Self) void {
        self.mu.unlock();
    }

    fn addCommand(self: *Self, cmd: Command) *Self {
        var copy = cmd;
        copy.order = self.event_count;
        self.event_count += 1;

        self.commands.append(copy) catch unreachable;
        return self;
    }

    fn quit(self: *Self) *Self {
        return self.addCommand(.{
            .req = (proto.QuitRequest{}).req(),
        });
    }

    fn addCondition(self: *Self, cond: Condition) *Self {
        var copy = cond;
        copy.order = self.event_count;
        self.event_count += 1;

        self.conditions.append(copy) catch unreachable;
        return self;
    }

    fn trim(s: String, sub: String) String {
        return mem.trimLeft(u8, s, sub);
    }

    fn run(self: *Self, sim_name: String) !void {
        const name = trim(trim(sim_name, "test"), ".");

        log.infof("{s}[TEST]{s} {s}", .{
            logging.Color.Blue.str(),
            logging.Color.Reset.str(),
            name,
        });

        if (self.skip) {
            log.warnf("{s}[SKIP]{s} {s}", .{
                logging.Color.Yellow.str(),
                logging.Color.Reset.str(),
                name,
            });
            return;
        }

        errdefer |err| log.errf("{s}[FAIL]{s} {s}: {!}", .{
            logging.Color.Red.str(),
            logging.Color.Reset.str(),
            name,
            err,
        });

        const serve_reqs = try self.dbg.serveRequestsForever();
        serve_reqs.detach();

        for (0..self.max_iterations) |tick_ndx| {
            const done = try self.tick();
            if (done) break;

            if (tick_ndx >= self.max_iterations - 1) {
                return error.MaxIterationsReached;
            }
        }

        for (self.conditions.items) |cond| {
            if (!cond.satisfied) {
                log.errf("condition not satisfied due to test timeout: {s}", .{cond.desc});
                return error.ConditionTimeout;
            }
        }

        log.infof("{s}[PASS]{s} {s}", .{
            logging.Color.Green.str(),
            logging.Color.Reset.str(),
            name,
        });
    }

    fn tick(self: *Self) !bool {
        defer self.tick_count += 1;

        // send the Commands that are ready, if any
        for (self.commands.items) |*cmd| {
            if (cmd.send_after_ticks < 0 or self.last_event != cmd.order) continue;

            defer cmd.send_after_ticks -= 1;

            const send = self.tick_count - self.last_event_tick >= cmd.send_after_ticks;
            if (send) {
                self.last_event += 1;
                self.last_event_tick = self.tick_count;
                cmd.send_after_ticks = 0;
                self.state.dbg.enqueueRequest(cmd.req);
            }
        }

        self.state.update();

        // check the Conditions that are ready, if any
        var skip = false;
        for (self.conditions.items) |*cond| {
            if (skip or cond.satisfied or self.last_event != cond.order) continue;

            const timeout = self.tick_count - self.last_event_tick >= cond.max_ticks;
            if (timeout and !cond.satisfied) {
                log.errf("condition not satisfied due to timeout: {s}", .{cond.desc});
                return error.ConditionTimeout;
            }

            if (cond.wait_for_ticks > 0) {
                cond.wait_for_ticks -= 1;
                continue;
            }
            defer cond.max_ticks -= 1;

            if (cond.cond(self)) |c| {
                if (c) {
                    self.last_event += 1;
                    self.last_event_tick = self.tick_count;
                    cond.satisfied = true;
                    log.infof("condition satisfied: {s}", .{cond.desc});
                } else {
                    log.errf("condition failed: {s}", .{cond.desc});
                    return error.ConditionFailed;
                }
            } else {
                // we should always decrement the counters on every Condition, but
                // we should only check to see if conditions are satisified in the
                // sequence in which they're declared
                skip = true;
            }
        }

        self.last_frame_render_micros = GUI.frameRateLimit(self.last_frame_render_micros);
        return self.state.dbg.shuttingDown();
    }
};

/// Indicates a request that will be send from the client to the debugger
const Command = struct {
    /// The number of ticks to wait before sending the request
    send_after_ticks: i32 = 0,

    /// Whether or not `max_ticks` is relative to the start of the simualtion
    /// or since the last successful condition
    ticks_relative_to_start: bool = false,

    /// The request to send to the debugger
    req: proto.Request,

    /// Tracks the sequence in which this Command was submitted
    order: usize = 0,
};

/// Indicates a test check that must be true
const Condition = struct {
    /// Internal field that should not be set when declaring a Condition
    satisfied: bool = false,

    /// The number of ticks to wait before this Condition must be true
    max_ticks: i32,

    /// The number of ticks to wait before attempting to check this Condition
    wait_for_ticks: i32 = 0,

    /// Helpful text for this condition that will be logged
    desc: String,

    /// Returns null if the condition is not yet ready for evaluation. If it returns
    /// a non-null value, true indicates the test has passed, false indicates failure.
    cond: *const fn (sim: *Simulator) ?bool,

    /// Tracks the sequence in which this Condition was submitted
    order: usize = 0,
};

/// @TODO (jrc): detach the simulator loop from the real GUI's frame limit so we can handle
/// variable refresh rate displays, or just displays at something other than 60fps
fn msToTicks(ms: i32) i32 {
    const fps = @as(i32, GUI.MaxFPS);
    const ms_per_frame = math.divCeil(i32, 1000, fps) catch unreachable;
    return math.divCeil(i32, ms, ms_per_frame) catch unreachable;
}

test "msToTicks" {
    try t.expectEqual(@as(i32, 0), msToTicks(0));
    try t.expectEqual(@as(i32, 1), msToTicks(1));
    try t.expectEqual(@as(i32, 2), msToTicks(20));
}

fn falseWithErr(comptime fmt: String, args: anytype) bool {
    log.errf(fmt, args);
    return false;
}

fn check(cond: bool, desc: String) bool {
    if (!cond) log.errf("condition failed: {s}", .{desc});
    return cond;
}

fn checkeq(comptime T: type, expected: T, actual: T, desc: String) bool {
    const msg = "condition failed, {s}'s are not equal: {s}";
    const args = .{ @typeName(T), desc };

    if (T == String) {
        if (!mem.eql(u8, expected, actual)) {
            log.errf(msg, args);
            log.errf("want: {s}", .{expected});
            log.errf("got: {s}", .{actual});
            return false;
        }
    } else {
        if (expected != actual) {
            log.errf(msg, args);
            log.errf("want: {any}", .{expected});
            log.errf("got: {any}", .{actual});
            return false;
        }
    }

    return true;
}

fn checkstr(str_cache: *strings.Cache, expected: String, actual: strings.Hash, desc: String) bool {
    if (str_cache.get(actual)) |str| {
        return checkeq(String, expected, str, desc);
    }

    log.errf("condition failed, string with hash 0x{x} not found in cache", .{actual});
    return false;
}

test "sim:cfastloop" {
    //
    // Tests the ability to load a binary's debug symbols, set some breakpoints, continue
    // execution, make sure we hit those breakpoints, and other basic operations on a simple
    // C program
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const cfastloop_main_c_hash = try fileHash(t.allocator, "assets/cfastloop/main.c");

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = "assets/cfastloop/out" }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return check(target.compile_units.len == 1, "must have one compilation unit") and
                        check(target.compile_units[0].ranges.len == 1, "must have one address range") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cfastloop_main_c_hash,
            .line = types.SourceLine.from(13),
        }}}).req(),
    })

    // launch subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.LaunchSubordinateRequest{
            .path = "assets/cfastloop/out",
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must be launched",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate != null) return true;
                return null;
            }
        }.cond,
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                const bp = blk: {
                    if (s.dbg.data.state.breakpoints.items.len == 0) return null;
                    break :blk s.dbg.data.state.breakpoints.items[0];
                };

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC");
                }

                return null;
            }
        }.cond,
    })

    // toggle off the breakpoint
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ToggleBreakpointRequest{ .id = types.BID.from(1) }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(250),
        .desc = "the breakpoint must be toggled off",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                const l = s.dbg.data.state.breakpoints.items.len;
                if (!checkeq(usize, 1, l, "one breakpoint must be set")) {
                    return false;
                }

                const bp = s.dbg.data.state.breakpoints.items[0];
                if (!bp.flags.active) return true;

                return null;
            }
        }.cond,
    })

    // continue execution
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .desc = "subordinate must be running after continue, and should not be stopped at the breakpoint that has been toggled off",
        .max_ticks = msToTicks(500),
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                // @TODO (jrc): can we ensure that we haven't hit the breakpoint for X iterations?
                if (s.dbg.data.subordinate.?.paused == null) return true;
                return null;
            }
        }.cond,
    })

    // toggle the breakpoint back on while the subordinate is executing
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ToggleBreakpointRequest{ .id = types.BID.from(1) }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(500),
        .desc = "the breakpoint must be toggled back on and the subordinate must be stopped at it",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                const bp = blk :{
                    if (!checkeq(usize, 1, s.dbg.data.state.breakpoints.items.len, "one breakpoint must be set")) return false;
                    if (!s.dbg.data.state.breakpoints.items[0].flags.active) return null;

                    break :blk s.dbg.data.state.breakpoints.items[0];
                };

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC");
                }
                return null;
            }
        }.cond,
    })

    // delete the breakpoint and continue execution
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .bid = types.BID.from(1) }}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(1000),
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "breakpoint must have been deleted and the subordinate must be executing",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.state.breakpoints.items.len > 0) return null;

                // @TODO (jrc): can we ensure that we haven't hit the breakpoint for X iterations?
                if (s.dbg.data.subordinate.?.paused == null) return true;

                return null;
            }
        }.cond,
    })

    // kill subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.KillSubordinateRequest{}).req(),
    })
    .addCondition(.{
        .desc = "subordinate must have been killed",
        .max_ticks = msToTicks(1000),
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate == null) return true;
                return null;
            }
        }.cond,
    })

    // launch subordinate again
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.LaunchSubordinateRequest{
            .path = "assets/cfastloop/out",
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must be launched again",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate != null) return true;
                return null;
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

test "sim:zigprint" {
    //
    // Tests the ability to render various variable values in a Zig program that tests
    // one of every primitive type, and a couple other types
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const exe_path = "assets/zigprint/out";
    const zigprint_main_zig_hash = try fileHash(t.allocator, "assets/zigprint/main.zig");

    const expected_output_len = 873;

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{ .req = (proto.LoadSymbolsRequest{
        .path = exe_path,
    }).req() })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 2, target.compile_units.len, "must have two compile units") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = zigprint_main_zig_hash,
            .line = types.SourceLine.from(145),
        }}}).req(),
    })

    // launch subordinate and ensure it hits the breakpoint
    .addCommand(.{
        .send_after_ticks = msToTicks(250),
        .req = (proto.LaunchSubordinateRequest{
            .path = exe_path,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                const bp = blk: {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate == null) return null;

                    if (s.dbg.data.state.breakpoints.items.len == 0) return null;
                    break :blk s.dbg.data.state.breakpoints.items[0];
                };

                s.state.primary.addWatchValue("a") catch unreachable;
                defer s.state.primary.watch_vars.clearAndFree();

                if (s.state.getStateSnapshot(s.arena.allocator())) |ss| {
                    if (ss.state.paused) |paused| {
                        if (!checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC"))
                            return false;

                        s.state.subordinate_output_mu.lock();
                        defer s.state.subordinate_output_mu.unlock();

                        // wait for output to arrive
                        if (s.state.subordinate_output.len == 0) return null;

                        // spot check a few fields
                        const num_locals = 56;
                        if (checkeq(usize, 4, s.state.subordinate_output.len, "unexpected program output len") and
                            checkeq(usize, num_locals, paused.locals.len, "unexpected number of local variables") and
                            checkeq(usize, num_locals, paused.locals.len, "unexpected number of local variable expression results") and
                            checkeq(usize, 1, paused.watches.len, "unexpected number of watch expressions") and
                            checkeq(String, "a", paused.strings.get(paused.watches[0].expression) orelse "", "first watch expression was incorrect") and
                            checkeq(String, "b", paused.strings.get(paused.locals[1].expression) orelse "", "second local expression was incorrect")) {

                            {
                                // test rendering a basic string
                                const ao = paused.getLocalByName("ao") orelse return falseWithErr("unable to get local \"ao\"", .{});
                                const data_hash = ao.fields[0].data orelse return falseWithErr("data not set on variable \"ao\"", .{});
                                if (!checkstr(paused.strings, "abcd", data_hash, "unexpected render value for field \"ao\"")) {
                                    return false;
                                }
                            }

                            {
                                // test rendering an opaque pointer
                                const ax = paused.getLocalByName("ax") orelse return falseWithErr("unable to get local \"ax\"", .{});
                                if (ax.fields[0].data != null) return falseWithErr("data should not set on variable \"ax\"", .{});
                                if (ax.fields[0].address == null) return falseWithErr("address should be set on variable \"ax\"", .{});
                                if (!checkeq(types.Address, types.Address.from(0x123), ax.fields[0].address.?, "unexpected address of \"ax\"")) return false;
                            }

                            {
                                // test rendering an array of u32's
                                const at = paused.getLocalByName("at") orelse return falseWithErr("unable to get local \"at\"", .{});
                                const field = at.fields[0];
                                if (field.encoding != .array) {
                                    log.errf("variable \"at\" encoding was not an array, got {s}", .{@tagName(field.encoding)});
                                    return false;
                                }

                                if (!checkeq(usize, 5, field.encoding.array.items.len, "unexpected number of array items for \"at\"")) {
                                    return false;
                                }

                                for (field.encoding.array.items, 0..) |field_ndx, i| {
                                    const item = at.fields[field_ndx.int()];
                                    if (item.encoding != .primitive) {
                                        log.errf("expected element at ndx {d} of \"at\" to be a primitive, got {s}", .{
                                            field_ndx,
                                            @tagName(item.encoding),
                                        });
                                        return false;
                                    }
                                    if (!checkeq(
                                        types.PrimitiveTypeEncoding,
                                        .unsigned,
                                        item.encoding.primitive.encoding,
                                        "item element encoding should be an unsigned primitive for \"at\"",
                                    )) return false;

                                    const data_hash = at.fields[field_ndx.int()].data orelse return falseWithErr(
                                        "unable to get data for \"at\" at field ndx {d}",
                                        .{field_ndx},
                                    );

                                    const i_u8: u8 = @intCast(i);
                                    const expected_char: u8 = '1' + i_u8;
                                    if (!checkstr(paused.strings, &.{expected_char, 0, 0, 0}, data_hash, "unexpected data for variable \"at\"")) return false;
                                }
                            }

                            {
                                // test rendering a simple struct
                                const ap = paused.getLocalByName("ap") orelse return falseWithErr("unable to get local \"ap\"", .{});
                                const field = ap.fields[0];
                                if (field.encoding != .@"struct") {
                                    log.errf("variable \"ap\" encoding was not a struct, got {s}", .{@tagName(field.encoding)});
                                    return false;
                                }

                                if (!checkeq(usize, 2, field.encoding.@"struct".members.len, "unexpected number of struct members for \"ap\"")) {
                                    return false;
                                }

                                // check the struct members
                                if (!checkNestedZigStructMembers(paused, ap)) return false;

                                {
                                    // check `field_a`
                                    const member = mem: {
                                        for (ap.fields) |f| {
                                            const name = paused.strings.get(f.name orelse 0).?;
                                            if (strings.eql(name, "field_a")) break :mem f;
                                        }
                                        log.err("\"field_a\" not found in struct \"ap\"");
                                        return false;
                                    };

                                    if (member.encoding != .primitive) {
                                        log.errf("\"ap.field_a\" was not a primitive, got {s}", .{@tagName(member.encoding)});
                                        return false;
                                    }
                                    if (member.encoding.primitive.encoding != .signed) {
                                        log.errf("\"ap.field_a\" was not a signed integer, got {s}", .{@tagName(member.encoding.primitive.encoding)});
                                        return false;
                                    }

                                    if (!checkstr(paused.strings, &.{123, 0, 0, 0}, member.data.?, "incorrect value for \"ap.field_a\"")) return false;
                                }

                            }

                            {
                                // test rendering a nested struct
                                const bb = paused.getLocalByName("bb") orelse return falseWithErr("unable to get local \"bb\"", .{});
                                const field = bb.fields[0];
                                if (field.encoding != .@"struct") {
                                    log.errf("variable \"bb\" encoding was not a struct, got {s}", .{@tagName(field.encoding)});
                                    return false;
                                }

                                if (!checkeq(usize, 2, field.encoding.@"struct".members.len, "unexpected number of struct members for \"bb\"")) {
                                    return false;
                                }

                                {
                                    // check the simple integer field (not nested)
                                    const member = mem: {
                                        for (bb.fields) |f| {
                                            const name = paused.strings.get(f.name orelse 0).?;
                                            if (strings.eql(name, "numeric")) break :mem f;
                                        }
                                        log.err("\"numeric\" not found in struct \"bb\"");
                                        return false;
                                    };

                                    if (member.encoding != .primitive) {
                                        log.errf("\"bb.numeric\" was not a primitive, got {s}", .{@tagName(member.encoding)});
                                        return false;
                                    }
                                    if (member.encoding.primitive.encoding != .signed) {
                                        log.errf("\"bb.numeric\" was not a signed integer, got {s}", .{@tagName(member.encoding.primitive.encoding)});
                                        return false;
                                    }

                                    if (!checkstr(paused.strings, &.{200, 1, 0, 0}, member.data.?, "incorrect value for \"bb.numeric\"")) return false;
                                }

                                // check `nested`, which is a struct within a struct
                                if (!checkNestedZigStructMembers(paused, bb)) return false;
                            }

                            {
                                // check rendering an enum value
                                const aw = paused.getLocalByName("aw") orelse return falseWithErr("unable to get local \"aw\"", .{});
                                const first = aw.fields[0];
                                const second = aw.fields[1];
                                if (first.encoding != .@"enum") {
                                    log.errf("variable \"aw\" encoding was not an enum, got {s}", .{@tagName(first.encoding)});
                                    return false;
                                }
                                if (second.encoding != .primitive) {
                                    log.errf("variable \"aw\" value encoding was not primitive, got {s}", .{@tagName(second.encoding)});
                                    return false;
                                }

                                if (!check(second.data != null, "enum data must not be null")) return false;
                                if (paused.strings.get(second.data.?)) |enum_data| {
                                    const enum_val = mem.readVarInt(i128, enum_data, .little);
                                    if (!checkeq(i128, 100, enum_val, "unexpected enum value \"aw\"")) return false;
                                }

                                if (!check(second.name != null, "enum name must not be null") or
                                    !checkstr(paused.strings, "final", second.name.?, "unexpected name for enum \"aw\"")) {
                                    return false;
                                }
                            }

                            {
                                // check rendering a negative enum value
                                const au = paused.getLocalByName("au") orelse return falseWithErr("unable to get local \"au\"", .{});
                                const first = au.fields[0];
                                const second = au.fields[1];
                                if (first.encoding != .@"enum") {
                                    log.errf("variable \"au\" encoding was not an enum, got {s}", .{@tagName(first.encoding)});
                                    return false;
                                }
                                if (second.encoding != .primitive) {
                                    log.errf("variable \"au\" value encoding was not primitive, got {s}", .{@tagName(second.encoding)});
                                    return false;
                                }

                                if (!check(second.data != null, "enum data must not be null")) return false;
                                if (paused.strings.get(second.data.?)) |enum_data| {
                                    const enum_val = mem.readVarInt(i8, enum_data, .little);
                                    if (!checkeq(i128, -1, enum_val, "unexpected enum value \"au\"")) return false;
                                }

                                if (!check(second.name != null, "enum name must not be null") or
                                    !checkstr(paused.strings, "negative", second.name.?, "unexpected name for enum \"av\"")) {
                                    return false;
                                }
                            }

                            return true;
                        }

                        // we return null here because it might be the case that the capture stdout thread
                        // has not yet received its data and passed it to the GUI
                        return null;
                    }
                } else |err| {
                    log.errf("unable to get state snapshot: {!}", .{err});
                    return false;
                }

                return null;
            }

        }.cond,
    })

    // continue execution and let it run to the end
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .desc = "subordinate must have finished execution and all subordinate output must be displayed",
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(4000),
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate != null) return null;
                }

                {
                    s.state.subordinate_output_mu.lock();
                    defer s.state.subordinate_output_mu.unlock();

                    if (expected_output_len == s.state.subordinate_output.len) {
                        return true;
                    }
                }

                return null;
            }
        }.cond,
    })

    // delete the breakpoint and run the end again
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .bid = types.BID.from(1) }}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = exe_path,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .desc = "subordinate must have finished execution and all subordinate output must be displayed on the second run",
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(4000),
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate != null) return null;

                s.state.subordinate_output_mu.lock();
                defer s.state.subordinate_output_mu.unlock();

                if (expected_output_len == s.state.subordinate_output.len) {
                    return true;
                }

                return null;
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

fn checkNestedZigStructMembers(paused: types.PauseData, res: types.ExpressionResult) bool {
    {
        // check `field_a`
        const field_a = mem: {
            for (res.fields) |f| {
                const name = paused.strings.get(f.name orelse 0).?;
                if (strings.eql(name, "field_a")) break :mem f;
            }
            log.err("\"field_a\" not found in struct \"bb.nested\"");
            return false;
        };

        if (field_a.encoding != .primitive) {
            log.errf("\"bb.nested.field_a\" was not a primitive, got {s}", .{@tagName(field_a.encoding)});
            return false;
        }
        if (field_a.encoding.primitive.encoding != .signed) {
            log.errf("\"bb.nested.field_a\" was not a signed integer, got {s}", .{@tagName(field_a.encoding.primitive.encoding)});
            return false;
        }

        if (!checkstr(paused.strings, &.{ 123, 0, 0, 0 }, field_a.data.?, "incorrect value for \"bb.nested.field_a\"")) return false;
    }

    {
        // check `field_b`
        const member = mem: {
            for (res.fields) |f| {
                const name = paused.strings.get(f.name orelse 0).?;
                if (strings.eql(name, "field_b")) break :mem f;
            }
            log.err("\"field_b\" not found in struct \"ap\"");
            return false;
        };

        if (member.encoding != .primitive) {
            log.errf("\"ap.field_b\" was not a primitive, got {s}", .{@tagName(member.encoding)});
            return false;
        }
        if (member.encoding.primitive.encoding != .string) {
            log.errf("\"ap.field_b\" was not a string, got {s}", .{@tagName(member.encoding.primitive.encoding)});
            return false;
        }

        if (!checkstr(paused.strings, "this is field_b", member.data.?, "incorrect value for \"ap.field_b\"")) return false;
    }

    return true;
}

test "sim:cmulticu" {
    //
    // Tests basic backtrace retrieval in a C program spread across multiple compilation units.
    // First, we set 3 breakpoints and ensure we can continue execution through multiple CUs.
    // Then, we set a breakpoint in one CU, then step in to the other CU, then step out back
    // to the original CU and step until `my_struct` has a value and confirm that it renders.
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const cmulticu_main_c_hash = try fileHash(t.allocator, "assets/cmulticu/main.c");
    const cmulticu_second_c_hash = try fileHash(t.allocator, "assets/cmulticu/second.c");

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = "assets/cmulticu/out" }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 2, target.compile_units.len, "there must be two compile units") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set some breakpoints
    .addCommand(.{
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cmulticu_main_c_hash,
            .line = types.SourceLine.from(4),
        }}}).req(),
    })
    .addCommand(.{
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cmulticu_main_c_hash,
            .line = types.SourceLine.from(11),
        }}}).req(),
    })
    .addCommand(.{
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cmulticu_second_c_hash,
            .line = types.SourceLine.from(4),
        }}}).req(),
    })

    // launch subordinate
    .addCommand(.{
        .send_after_ticks = msToTicks(2000),
        .req = (proto.LaunchSubordinateRequest{
            .path = "assets/cmulticu/out",
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must be launched",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate != null) return true;
                return null;
            }
        }.cond,
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the first breakpoint in main.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.state.breakpoints.items.len < 2) return null;

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.source_location != null and paused.source_location.?.line.eql(types.SourceLine.from(4)) and
                        check(paused.stack_frames.len >= 1, "there must be at least 1 stack frame") and
                        checkstr(paused.strings, "main", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0");
                }

                return null;
            }
        }.cond,
    })

    // continue execution
    .addCommand(.{
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint in second.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.state.breakpoints.items.len < 2) return null;

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.source_location != null and paused.source_location.?.line.eql(types.SourceLine.from(4)) and
                        check(paused.stack_frames.len >= 2, "there must be at least 2 stack frames") and
                        checkstr(paused.strings, "MyFunc", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0") and
                        checkstr(paused.strings, "main", paused.stack_frames[1].name orelse 0, "incorrect stack frame name at index 1");
                }

                return null;
            }
        }.cond,
    })

    // continue execution
    .addCommand(.{
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the second breakpoint in main.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate) |sub| {
                    if (sub.paused) |paused| {
                        return check(paused.stack_frames.len >= 1, "there must be at least one stack frame") and
                            checkstr(paused.strings, "main", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0");
                    }
                }

                return null;
            }
        }.cond,
    })

    // kill the subordinate
    .addCommand(.{
        .req = (proto.KillSubordinateRequest{}).req(),
    })

    // launch subordinate again
    .addCommand(.{
        .send_after_ticks = msToTicks(2000),
        .req = (proto.LaunchSubordinateRequest{
            .path = "assets/cmulticu/out",
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must be launched",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate != null) return true;
                return null;
            }
        }.cond,
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint in main.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.source_location != null and paused.source_location.?.line.eql(types.SourceLine.from(4)) and
                        check(paused.stack_frames.len >= 1, "there must be at least 1 stack frame") and
                        checkstr(paused.strings, "main", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0");
                }

                return null;
            }
        }.cond,
    })

    // step into MyFunc in the second compile unit
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to second.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.source_location != null and paused.source_location.?.line.eql(types.SourceLine.from(4)) and
                        check(paused.stack_frames.len >= 2, "there must be at least 2 stack frames") and
                        checkstr(paused.strings, "MyFunc", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0") and
                        checkstr(paused.strings, "main", paused.stack_frames[1].name orelse 0, "incorrect stack frame name at index 1");
                }

                return null;
            }
        }.cond,
    })

    // step out, back in to main.c
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out back in to main.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.source_location != null and paused.source_location.?.line.eql(types.SourceLine.from(5)) and
                        check(paused.stack_frames.len >= 1, "there must be at least 1 stack frame") and
                        checkstr(paused.strings, "main", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0");
                }

                return null;
            }
        }.cond,
    })

    // step until line 10
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "the subordinate must be paused at line 10 in main.c",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    if (paused.source_location != null and paused.source_location.?.line.eql(types.SourceLine.from(10))) {
                        return true;
                    }
                }

                return null;
            }
        }.cond,
    })

    // step until line 11
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "the correct value for my_struct must be rendered",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate.?.paused) |paused| {
                        // we've not yet hit the appropriate line of code
                        if (!checkeq(types.SourceLine, types.SourceLine.from(11), paused.source_location.?.line, "not paused at line 11")) {
                            return false;
                        }
                    }
                }

                if (s.state.getStateSnapshot(s.arena.allocator())) |ss| {
                    if (ss.state.paused) |paused| {
                        if (paused.locals.len != 1) return null;

                        return true;
                    }
                } else |err| {
                    log.errf("unable to get state snapshot: {!}", .{err});
                    return false;
                }

                return null;
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

test "sim:cbacktrace" {
    //
    // Tests basic backtrace retrieval in a C program with multiple levels of call stack depth
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const cbacktrace_main_c_hash = try fileHash(t.allocator, "assets/cbacktrace/main.c");

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = cbacktrace_exe }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 1, target.compile_units.len, "incorrect number of compile units") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set some breakpoints
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cbacktrace_main_c_hash,
            .line = cbacktrace_main_loc,
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cbacktrace_main_c_hash,
            .line = cbacktrace_c_loc,
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cbacktrace_main_c_hash,
            .line = cbacktrace_e_loc,
        }}}).req(),
    })

    // launch subordinate
    .addCommand(.{
        .send_after_ticks = msToTicks(1000),
        .req = (proto.LaunchSubordinateRequest{
            .path = cbacktrace_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must be launched",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate != null) return true;
                return null;
            }
        }.cond,
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint in main()",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                const bp = blk: {
                    for (s.dbg.data.state.breakpoints.items) |b| {
                        if (b.source_location) |src| {
                            if (src.line == cbacktrace_main_loc) break :blk b;
                        }
                    }
                    return null;
                };

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC") and
                        check(paused.stack_frames.len >= 1, "incorrect number of stack frames") and
                        checkstr(paused.strings, "main", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0");
                }

                return null;
            }
        }.cond,
    })

    // continue execution
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(4000),
        .desc = "subordinate must have hit the breakpoint in FuncC()",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                const bp = blk: {
                    for (s.dbg.data.state.breakpoints.items) |b| {
                        if (b.source_location) |src| {
                            if (src.line.eql(cbacktrace_c_loc)) break :blk b;
                        }
                    }
                    return null;
                };

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC") and
                        check(paused.stack_frames.len >= 4, "incorrect number of stack frames") and
                        checkstr(paused.strings, "FuncC", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0") and
                        checkstr(paused.strings, "FuncB", paused.stack_frames[1].name orelse 0, "incorrect stack frame name at index 1") and
                        checkstr(paused.strings, "FuncA", paused.stack_frames[2].name orelse 0, "incorrect stack frame name at index 2") and
                        checkstr(paused.strings, "main", paused.stack_frames[3].name orelse 0, "incorrect stack frame name at index 3");
                }

                return null;
            }
        }.cond,
    })

    // continue execution
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(1000),
        .max_ticks = msToTicks(4000),
        .desc = "subordinate must have hit the breakpoint in FuncE()",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                const bp = blk: {
                    for (s.dbg.data.state.breakpoints.items) |b| {
                        if (b.source_location) |src| {
                            if (src.line.eql(cbacktrace_e_loc)) break :blk b;
                        }
                    }
                    return null;
                };

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC") and
                        check(paused.stack_frames.len >= 6, "incorrect number of stack frames") and
                        checkstr(paused.strings, "FuncE", paused.stack_frames[0].name orelse 0, "incorrect stack frame name at index 0") and
                        checkstr(paused.strings, "FuncD", paused.stack_frames[1].name orelse 0, "incorrect stack frame name at index 1") and
                        checkstr(paused.strings, "FuncC", paused.stack_frames[2].name orelse 0, "incorrect stack frame name at index 2") and
                        checkstr(paused.strings, "FuncB", paused.stack_frames[3].name orelse 0, "incorrect stack frame name at index 3") and
                        checkstr(paused.strings, "FuncA", paused.stack_frames[4].name orelse 0, "incorrect stack frame name at index 4") and
                        checkstr(paused.strings, "main", paused.stack_frames[5].name orelse 0, "incorrect stack frame name at index 5");
                }

                return null;
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

const zigbacktrace_exe = "assets/zigbacktrace/out";
const zigbacktrace_main_zig = "assets/zigbacktrace/main.zig";

const zigbacktrace_main_loc = types.SourceLine.from(28); // main()
const zigbacktrace_a_loc = types.SourceLine.from(23); // funcA()
const zigbacktrace_b_loc = types.SourceLine.from(18); // funcB()
const zigbacktrace_c_loc = types.SourceLine.from(13); // funcC()
const zigbacktrace_d_loc = types.SourceLine.from(8); // funcD()
const zigbacktrace_e_loc = types.SourceLine.from(4); // funcE()

fn checkZigBacktraceLine(s: *Simulator, line: types.SourceLine, stack_depth: usize) ?bool {
    s.dbg.data.mu.lock();
    defer s.dbg.data.mu.unlock();

    if (s.dbg.data.subordinate) |sub| {
        if (sub.paused) |paused| {
            if (paused.source_location == null) {
                log.err("subordinate is stopped at a PC that has no source line");
                return false;
            }
            const src = paused.source_location.?;

            // must be recalculated because its value is not runtime-known
            const fhash = fileHash(t.allocator, zigbacktrace_main_zig) catch unreachable;

            return checkeq(file_utils.Hash, fhash, src.file_hash, "stopped in a file other than main.zig") and
                checkeq(types.SourceLine, line, src.line, "stopped at the wrong line in main.zig") and
                checkeq(usize, stack_depth, paused.stack_frames.len, "incorrect stacktrace depth");
        }
    }

    return null;
}

const cbacktrace_exe = "assets/cbacktrace/out";
const cbacktrace_main_c = "assets/cbacktrace/main.c";

const cbacktrace_main_loc = types.SourceLine.from(34); // main()
const cbacktrace_a_loc = types.SourceLine.from(23); // FuncA()
const cbacktrace_b_loc = types.SourceLine.from(18); // FuncB()
const cbacktrace_c_loc = types.SourceLine.from(13); // FuncC()
const cbacktrace_d_loc = types.SourceLine.from(8); // FuncD()
const cbacktrace_e_loc = types.SourceLine.from(4); // FuncE()
const cbacktrace_f_loc = types.SourceLine.from(29); // FuncF()

var cbacktrace_initial_stack_depth: usize = 0;

fn checkCBacktraceLine(s: *Simulator, line: types.SourceLine, stack_depth: usize) ?bool {
    s.dbg.data.mu.lock();
    defer s.dbg.data.mu.unlock();

    if (s.dbg.data.subordinate) |sub| {
        if (sub.paused) |paused| {
            if (paused.source_location == null) {
                log.err("subordinate is stopped at a PC that has unknown source location");
                return false;
            }
            const src = paused.source_location.?;

            // must be recalculated because its value is not runtime-known
            const fhash = fileHash(t.allocator, cbacktrace_main_c) catch unreachable;

            return checkeq(file_utils.Hash, fhash, src.file_hash, "stopped in a file other than main.c") and
                checkeq(types.SourceLine, line, src.line, "stopped at the wrong line in main.c") and
                checkeq(usize, cbacktrace_initial_stack_depth + stack_depth, paused.stack_frames.len, "incorrect stacktrace depth");
        }
    }

    return null;
}

test "sim:step_over_until_end" {
    //
    // Tests step over in a Zig program (LLVM backend) all the way to the end of the program
    // and ensure that it stops correctly
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const zigbacktrace_main_zig_hash = try fileHash(t.allocator, zigbacktrace_main_zig);

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = zigbacktrace_exe }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 2, target.compile_units.len, "must have two compilation units") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint and launch the subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = zigbacktrace_main_zig_hash,
            .line = zigbacktrace_main_loc,
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = zigbacktrace_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })

    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(5000),
        .desc = "subordinate must have hit the breakpoint",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc, 3);
            }
        }.cond,
    })

    // step over once
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped over one function call",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc.addInt(1), 3);
            }
        }.cond,
    })

    // step over twice
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(5000),
        .desc = "subordinate must have stepped over two function calls",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc.addInt(3), 3);
            }
        }.cond,
    })

    // step until the final line
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(5000),
        .desc = "subordinate must have stepped until the final line of the program",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc.addInt(4), 3);
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

test "sim:step_over_returns_to_caller" {
    //
    // Tests the step over function to make sure it returns to the caller if the current function reaches its end
    //

    const cbacktrace_main_c_hash = try fileHash(t.allocator, cbacktrace_main_c);

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = cbacktrace_exe }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 1, target.compile_units.len, "must have one compilation unit") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint in FuncA and launch the subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cbacktrace_main_c_hash,
            .line = cbacktrace_a_loc,
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = cbacktrace_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })

    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint in FuncA",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate) |sub| {
                        if (sub.paused) |paused| {
                            // -1 because we're one deep from main()
                            cbacktrace_initial_stack_depth = paused.stack_frames.len - 1;
                        }
                    }
                }

                return checkCBacktraceLine(s, cbacktrace_a_loc, 1);
            }
        }.cond,
    })

    // step over until the end of the function
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped over (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkCBacktraceLine(s, cbacktrace_a_loc.addInt(1), 1);
            }
        }.cond,
    })

    // step out of FuncA and ensure we're back in main on the correct line
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped over (2)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkCBacktraceLine(s, cbacktrace_main_loc.addInt(1), 0);
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

test "sim:step_in_and_out" {
    //
    // Tests stepping in and out of functions in a Zig program (LLVM backend)
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const zigbacktrace_main_zig_hash = try fileHash(t.allocator, zigbacktrace_main_zig);

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = zigbacktrace_exe }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 2, target.compile_units.len, "must have two compilation units") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint and launch the subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = zigbacktrace_main_zig_hash,
            .line = zigbacktrace_main_loc,
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = zigbacktrace_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })

    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc, 3);
            }
        }.cond,
    })

    // step into funcA
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to funcA (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_a_loc, 4);
            }
        }.cond,
    })

    // step in to funcB
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to funcB (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_b_loc, 5);
            }
        }.cond,
    })

    // step in to funcC
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to funcC (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_c_loc, 6);
            }
        }.cond,
    })

    // step in to funcD
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to funcD (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_d_loc, 7);
            }
        }.cond,
    })

    // step in to funcE
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to funcE (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_e_loc, 8);
            }
        }.cond,
    })

    // step out to funcD
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out to funcD (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_d_loc.addInt(1), 7);
            }
        }.cond,
    })

    // step out to funcC
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out to funcC (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_c_loc.addInt(1), 6);
            }
        }.cond,
    })

    // step out to funcB
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out to funcB (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_b_loc.addInt(1), 5);
            }
        }.cond,
    })

    // step out to funcA
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out to funcA (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_a_loc.addInt(1), 4);
            }
        }.cond,
    })

    // step out to main
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out to main (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc.addInt(1), 3);
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

test "sim:step_in_then_over_then_out" {
    //
    // Tests stepping various in and out operations in a Zig program (LLVM backend)
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const zigbacktrace_main_zig_hash = try fileHash(t.allocator, zigbacktrace_main_zig);

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = zigbacktrace_exe }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 2, target.compile_units.len, "must have two compilation units") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint and launch the subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = zigbacktrace_main_zig_hash,
            .line = zigbacktrace_main_loc,
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = zigbacktrace_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })

    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc, 3);
            }
        }.cond,
    })

    // step into funcA
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to funcA (1)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_a_loc, 4);
            }
        }.cond,
    })

    // step over funcB
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped over funcB (2)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_a_loc.addInt(1), 4);
            }
        }.cond,
    })

    // step over again to return to main
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped over funcB (2)",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                return checkZigBacktraceLine(s, zigbacktrace_main_loc.addInt(1), 3);
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

const cprint_my_func_breakpoint_line = types.SourceLine.from(35);
const cprint_main_breakpoint_line = types.SourceLine.from(103);

test "sim:cprint" {
    //
    // Tests the ability to render various variable values in a C program that prints out
    // instances various data types
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    const exe_path = "assets/cprint/out";
    const cprint_main_c_hash = try fileHash(t.allocator, "assets/cprint/main.c");

    const expected_output_len = 522;

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{ .req = (proto.LoadSymbolsRequest{
        .path = exe_path,
    }).req() })
    .addCondition(.{
        .max_ticks = msToTicks(20000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 1, target.compile_units.len, "must have one compile unit") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint in my_func()
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cprint_main_c_hash,
            .line = cprint_my_func_breakpoint_line,
        }}}).req(),
    })

    // set a breakpoint in main()
    .addCommand(.{
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = cprint_main_c_hash,
            .line = cprint_main_breakpoint_line,
        }}}).req(),
    })

    // launch subordinate and ensure it hits the breakpoint in my_func()
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = exe_path,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint in my_func and rendered its variables correctly",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                const bp = blk: {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate == null) return null;

                    for (s.dbg.data.state.breakpoints.items) |bp| {
                        if (bp.source_location.?.line.eql(cprint_my_func_breakpoint_line)) {
                            break :blk bp;
                        }
                    }
                    return null;
                };

                if (s.state.getStateSnapshot(s.arena.allocator())) |ss| {
                    if (ss.state.paused == null) return null;
                    const paused = ss.state.paused.?;

                    if (!checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC"))
                        return false;

                    const num_locals = 3;
                    if (!checkeq(usize, num_locals, paused.locals.len, "unexpected number of local variables") or
                        !checkeq(String, "param", paused.strings.get(paused.locals[0].expression) orelse "", "first local expression was incorrect") or
                        !checkeq(String, "ts2", paused.strings.get(paused.locals[1].expression) orelse "", "second local expression was incorrect") or
                        !checkeq(String, "res", paused.strings.get(paused.locals[2].expression) orelse "", "third local expression was incorrect")) {
                        return false;
                    }

                    {
                        // spot check a field
                        const param = paused.getLocalByName("param") orelse return falseWithErr("unable to get local \"param\"", .{});
                        const data_hash = param.fields[0].data orelse return falseWithErr("data not set on variable \"param\"", .{});
                        const val = mem.readVarInt(u64, paused.getString(data_hash), .little);
                        if (!checkeq(u64, 19, val, "unexpected render value for field \"param\"")) {
                            return false;
                        }
                    }

                    return true;
                } else |err| {
                    log.errf("unable to get state snapshot: {!}", .{err});
                    return false;
                }

                return null;
            }
        }.cond,
    })

    // continue execution and make sure it hits the breakpoint in main()
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .wait_for_ticks = msToTicks(100),
        .desc = "subordinate must have hit the breakpoint in main and rendered its variables correctly",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                const bp = blk: {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate == null) return null;

                    for (s.dbg.data.state.breakpoints.items) |bp| {
                        if (bp.source_location.?.line.eql(cprint_main_breakpoint_line)) {
                            break :blk bp;
                        }
                    }
                    return null;
                };

                if (s.state.getStateSnapshot(s.arena.allocator())) |ss| {
                    if (ss.state.paused == null) return null;
                    const paused = ss.state.paused.?;

                    if (!checkeq(types.Address, bp.addr, paused.registers.pc(), "breakpoint addr must equal PC"))
                        return false;

                    // spot check a few fields
                    const num_locals = 30;
                    if (!checkeq(usize, num_locals, paused.locals.len, "unexpected number of local variables") or
                        !checkeq(String, "a", paused.strings.get(paused.locals[0].expression) orelse "", "first local expression was incorrect") or
                        !checkeq(String, "b", paused.strings.get(paused.locals[1].expression) orelse "", "second local expression was incorrect")) {
                        return false;
                    }

                    {
                        // test rendering a basic int
                        const c = paused.getLocalByName("c") orelse return falseWithErr("unable to get local \"c\"", .{});
                        const data_hash = c.fields[0].data orelse return falseWithErr("data not set on variable \"c\"", .{});
                        if (paused.strings.get(data_hash)) |buf| {
                            const val = mem.readVarInt(c_int, buf, .little);
                            if (!checkeq(c_int, 3, val, "unexpected value for local \"c\"")) return false;
                        } else {
                            log.err("local variable \"c\" data not found in string cache");
                            return false;
                        }
                    }

                    {
                        // test rendering a char*
                        const str = paused.getLocalByName("basic_str") orelse return falseWithErr("unable to get local \"basic_str\"", .{});
                        const data_hash = str.fields[0].data orelse return falseWithErr("data not set on variable \"basic_str\"", .{});
                        if (!checkstr(paused.strings, "Hello, world!", data_hash, "unexpected render value for field \"basic_str\"")) {
                            return false;
                        }
                    }

                    {
                        // test rendering a void*
                        const opaque_ptr = paused.getLocalByName("opaque_ptr") orelse return falseWithErr("unable to get local \"opaque_ptr\"", .{});
                        if (opaque_ptr.fields[0].data != null) return falseWithErr("data should not set on variable \"opaque_ptr\"", .{});
                        if (opaque_ptr.fields[0].address == null) return falseWithErr("address should be set on variable \"opaque_ptr\"", .{});
                    }

                    {
                        // test rendering an enum that's behind a typedef
                        const enum_three = paused.getLocalByName("enum_three") orelse return falseWithErr("unable to get local \"enum_three\"", .{});
                        const data_hash = enum_three.fields[1].data orelse return falseWithErr("data not set on variable \"enum_three\"", .{});
                        if (paused.strings.get(data_hash)) |buf| {
                            const val = mem.readVarInt(c_int, buf, .little);
                            if (!checkeq(c_int, 2, val, "unexpected value for local \"enum_three\"")) return false;
                        } else {
                            log.err("local variable \"enum_three\" data not found in string cache");
                            return false;
                        }
                    }

                    {
                        // test rendering a pointer to a primitive
                        const j_ptr = paused.getLocalByName("j_ptr") orelse return falseWithErr("unable to get local \"j_ptr\"", .{});
                        const data_hash = j_ptr.fields[0].data orelse return falseWithErr("data not set on variable \"j_ptr\"", .{});
                        const val = mem.readVarInt(u64, paused.getString(data_hash), .little);
                        if (!checkeq(u64, 10, val, "unexpected render value for field \"j_ptr\"")) {
                            return false;
                        }
                    }

                    {
                        // test rendering a pointer to a struct
                        const ts2 = paused.getLocalByName("ts2") orelse return falseWithErr("unable to get local \"ts2\"", .{});
                        if (!checkeq(usize, 3, ts2.fields.len, "unexpected number of fields on struct \"ts2\"")) return false;

                        {
                            // check the first struct member
                            const data_hash = ts2.fields[1].data orelse return falseWithErr("first member not set on variable \"ts2\"", .{});
                            const val = mem.readVarInt(u64, paused.getString(data_hash), .little);
                            if (!checkeq(u64, 15, val, "unexpected render value for first field on \"ts2\"")) {
                                return false;
                            }
                        }

                        {
                            // check the second struct member
                            const data_hash = ts2.fields[2].data orelse return falseWithErr("second member not set on variable \"ts2\"", .{});
                            const val = mem.readVarInt(u64, paused.getString(data_hash), .little);
                            if (!checkeq(u64, 16, val, "unexpected render value for second field on \"ts2\"")) {
                                return false;
                            }
                        }
                    }

                    {
                        // test rendering a stack-allocated array
                        const arr = paused.getLocalByName("arr") orelse return falseWithErr("unable to get local \"arr\"", .{});
                        if (!checkeq(usize, 15, arr.fields.len, "unexpected number of fields on struct \"arr\"")) return false;

                        {
                            // check the zero'th array element
                            const data_hash = arr.fields[1].data orelse return falseWithErr("first member not set on variable \"arr\"", .{});
                            const val = mem.readVarInt(u32, paused.getString(data_hash), .little);
                            const val_float: f32 = @bitCast(val);
                            if (!checkeq(f32, 1.23, val_float, "unexpected render value for zero'th element in \"arr\"")) {
                                return false;
                            }
                        }

                        {
                            // check the third array element
                            const data_hash = arr.fields[3].data orelse return falseWithErr("third member not set on variable \"arr\"", .{});
                            const val = mem.readVarInt(u32, paused.getString(data_hash), .little);
                            const val_float: f32 = @bitCast(val);
                            if (!checkeq(f32, 0, val_float, "unexpected render value for third element in \"arr\"")) {
                                return false;
                            }
                        }

                        {
                            // check the final array element
                            const data_hash = arr.fields[14].data orelse return falseWithErr("final member not set on variable \"arr\"", .{});
                            const val = mem.readVarInt(u32, paused.getString(data_hash), .little);
                            const val_float: f32 = @bitCast(val);
                            if (!checkeq(f32, 7.89, val_float, "unexpected render value for final element in \"arr\"")) {
                                return false;
                            }
                        }
                    }

                    return true;
                } else |err| {
                    log.errf("unable to get state snapshot: {!}", .{err});
                    return false;
                }

                return null;
            }
        }.cond,
    })

    // continue execution and let it run to the end
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.ContinueRequest{}).req(),
    })
    .addCondition(.{
        .desc = "subordinate must have finished execution and all subordinate output must be displayed",
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(4000),
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                {
                    s.dbg.data.mu.lock();
                    defer s.dbg.data.mu.unlock();

                    if (s.dbg.data.subordinate != null) return null;
                }

                {
                    s.state.subordinate_output_mu.lock();
                    defer s.state.subordinate_output_mu.unlock();

                    if (checkeq(usize, expected_output_len, s.state.subordinate_output.len, "unexpected program output length"))
                        return true;
                }

                return null;
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

// tracking this as a global so it's accessible from condition checks, and we need
// to track it because different C compiler version may give different stack depths
var crecursion_initial_stack_depth: usize = 0;

test "sim:crecursion" {
    //
    // Tests stepping through a recursive function
    //

    const sim = try Simulator.init(t.allocator);
    defer sim.deinit(t.allocator);

    // @TODO (jrc): fix recursive step out behavior and re-enable this test
    sim.skip = true;

    const crecursion_path = "assets/crecursion/main.c";
    const crecursion_main_c_hash = try fileHash(t.allocator, crecursion_path);
    const crecursion_exe = "assets/crecursion/out";
    const breakpoint_line = 20;

    // zig fmt: off
    sim.lock()

    // load symbols
    .addCommand(.{
        .req = (proto.LoadSymbolsRequest{ .path = crecursion_exe }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(4000),
        .desc = "debug symbols must be loaded",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.target) |target| {
                    return checkeq(usize, 1, target.compile_units.len, "must have one compilation unit") and
                        check(s.dbg.data.subordinate == null, "subordinate must not be launched");
                }

                return null;
            }
        }.cond,
    })

    // set a breakpoint and launch the subordinate
    .addCommand(.{
        .send_after_ticks = 1,
        .req = (proto.UpdateBreakpointRequest{ .loc = .{ .source = .{
            .file_hash = crecursion_main_c_hash,
            .line = types.SourceLine.from(breakpoint_line),
        }}}).req(),
    })
    .addCommand(.{
        .send_after_ticks = msToTicks(100),
        .req = (proto.LaunchSubordinateRequest{
            .path = crecursion_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have hit the breakpoint",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate) |sub| {
                    if (sub.paused) |paused| {
                        if (paused.source_location == null) {
                            log.err("subordinate is stopped at a PC that has no source location");
                            return false;
                        }
                        const src = paused.source_location.?;

                        const fhash = fileHash(t.allocator, crecursion_path) catch unreachable;
                        crecursion_initial_stack_depth = paused.stack_frames.len;

                        return checkeq(file_utils.Hash, fhash, src.file_hash, "stopped in a file other than main.c") and
                            checkeq(types.SourceLine, types.SourceLine.from(breakpoint_line), src.line, "stopped at the wrong line in main.c");
                    }
                }

                return null;
            }
        }.cond,
    })

    // step into function `Recursion`
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to the recursive function",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate) |sub| {
                    if (sub.paused) |paused| {
                        return checkeq(usize, crecursion_initial_stack_depth + 1, paused.stack_frames.len, "stack depth is incorrect");
                    }
                }

                return null;
            }
        }.cond,
    })

    // step four times until the depth pointer has been updated from zero to one
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have done the first step",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| return checkCRecursionDepth(paused, 0, 1);
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have done the second step",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| return checkCRecursionDepth(paused, 0, 1);
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have done the third step",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| return checkCRecursionDepth(paused, 0, 1);
                return null;
            }
        }.cond,
    })

    // after this step, depth should be set to one and the cursor should be on `Recursion`
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have done the fourth step and *depth should be 1",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| return checkCRecursionDepth(paused, 1, 1);
                return null;
            }
        }.cond,
    })

    // step in to `Recursion` for the second time
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to Recursion for the second time",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| return checkCRecursionDepth(paused, 1, 2);
                return null;
            }
        }.cond,
    })

    // step out of this call to `Recursion` and *depth should be MAX_DEPTH, which is 5
    //
    // @REF: RECURSIVE_STEP_BUG
    // @NOTE (jrc): there is a known bug where we stop at one instruction too early when we step out of a
    // recursive function where we are multiple calls deep, which means the user lands at a spot that is
    // typically not associated with a line of code. Why is this happening? Once we fix it, add a test
    // case here.
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .out_of}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped out of the second call to Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                // *depth == 1, and we're one function call deep
                if (s.dbg.data.subordinate.?.paused) |paused| return checkCRecursionDepth(paused, 5, 1);
                return null;
            }
        }.cond,
    })

    // kill the subordinate and re-launch for a second check
    .addCommand(.{
        .req = (proto.KillSubordinateRequest{}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have been killed",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate == null) return true;
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.LaunchSubordinateRequest{
            .path = crecursion_exe,
            .args = "",
            .stop_on_entry = false,
        }).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have been launched again and stopped at the ",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate) |sub| {
                    if (sub.paused) |paused| {
                        return paused.stack_frames.len == crecursion_initial_stack_depth;
                    }
                }
                return null;
            }
        }.cond,
    })

    // step into `Recursive`
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .into}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(250),
        .max_ticks = msToTicks(2000),
        .desc = "subordinate must have stepped in to Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth + 1;
                }
                return null;
            }
        }.cond,
    })

    // step over five times
    //
    // @SEE: RECURSIVE_STEP_BUG
    // in an ideal world, we would fix the bug that requires the final step
    // and this would only be four steps
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have stepped once in Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth + 1;
                }
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have stepped twice in Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth + 1;
                }
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have stepped three times in Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth + 1;
                }
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have stepped four times in Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth + 1;
                }
                return null;
            }
        }.cond,
    })
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have stepped five times in Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth + 1;
                }
                return null;
            }
        }.cond,
    })

    // the next step gets us back to main()
    .addCommand(.{
        .req = (proto.StepRequest{.step_type = .over}).req(),
    })
    .addCondition(.{
        .wait_for_ticks = msToTicks(100),
        .max_ticks = msToTicks(1000),
        .desc = "subordinate must have stepped out of Recursion",
        .cond = struct {
            fn cond(s: *Simulator) ?bool {
                s.dbg.data.mu.lock();
                defer s.dbg.data.mu.unlock();

                if (s.dbg.data.subordinate.?.paused) |paused| {
                    return paused.stack_frames.len == crecursion_initial_stack_depth;
                }
                return null;
            }
        }.cond,
    })

    .quit().unlock();
    // zig fmt: on

    try sim.run(@src().fn_name);
}

fn checkCRecursionDepth(paused: types.PauseData, expected_depth_var: u8, expected_stack_depth: usize) bool {
    const name = "depth";
    const ndx = blk: {
        for (paused.locals, 0..) |v, i| {
            const expr = paused.getString(v.expression);
            if (strings.eql(name, expr)) break :blk i;
        }

        log.errf("variable \"{s}\" not found in local scope", .{name});
        return false;
    };

    const expected_depth_buf = [_]u8{ expected_depth_var, 0, 0, 0 };

    const local = paused.locals[ndx];
    if (!checkeq(usize, 1, local.fields.len, "incorrect number of local expressions for variable \"depth\""))
        return false;

    const field = local.fields[0];
    return check(field.address != null, "depth variable must have a pointer value set") and
        check(field.data != null, "depth variable must have a pointer value set") and
        checkeq(String, &expected_depth_buf, paused.getString(field.data.?), "incorrect depth variable value") and
        checkeq(usize, crecursion_initial_stack_depth + expected_stack_depth, paused.stack_frames.len, "incorrect stack depth");
}
