const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const assert = std.debug.assert;
const atomic = std.atomic;
const AutoHashMapUnmanaged = std.AutoHashMapUnmanaged;
const fmt = std.fmt;
const heap = std.heap;
const math = std.math;
const mem = std.mem;
const Mutex = Thread.Mutex;
const Thread = std.Thread;
const ThreadSafeAllocator = heap.ThreadSafeAllocator;
const time = std.time;
const WaitGroup = Thread.WaitGroup;

const arch = @import("../arch.zig").arch;
const Child = @import("Child.zig");
const encoding = @import("encoding/encoding.zig");
const file = @import("../file.zig");
const flags = @import("../flags.zig");
const logging = @import("../logging.zig");
const proto = @import("proto.zig");
const Queue = @import("../queue.zig").Queue;
const safe = @import("../safe.zig");
const strings = @import("../strings.zig");
const String = strings.String;
const trace = @import("../trace.zig");
const types = @import("../types.zig");

const log = logging.Logger.init(logging.Region.Debugger);

pub const Adapter = switch (builtin.os.tag) {
    .linux => @import("../linux.zig").Adapter,
    else => @compileError("unsupported platform: " ++ @tagName(builtin.os.tag)),
};

pub const Debugger = DebuggerType(Adapter);

/// Stores all data that lives outside the scope of a single run of a subordinate process. For
/// instance, a user may set breakpoints even if the subordinate is not running, so store them
/// in this struct.
const State = struct {
    const Self = @This();

    shutdown_wg: WaitGroup = .{},
    shutting_down: atomic.Value(bool) = atomic.Value(bool).init(false),

    strings: *strings.Cache,

    breakpoints: ArrayListUnmanaged(types.Breakpoint) = .{},
    max_breakpoint_id: atomic.Value(u64) = atomic.Value(u64).init(1),

    watch_expressions: ArrayListUnmanaged(String) = .{},

    /// The address the user would like to display in the memory viewer window
    hex_window_address: ?types.Address = null,

    fn init(alloc: Allocator) !Self {
        return Self{
            .strings = try strings.Cache.init(alloc),
        };
    }

    fn deinit(self: *Self, alloc: Allocator) void {
        self.breakpoints.deinit(alloc);

        self.clearAndFreeWatchExpressions(alloc);
        self.watch_expressions.deinit(alloc);
    }

    fn nextBreakpointID(self: *Self) types.BID {
        return types.BID.from(self.max_breakpoint_id.fetchAdd(1, .seq_cst));
    }

    /// Takes a copy of `expression` in to `alloc`-owned memory
    fn addWatchExpression(self: *Self, alloc: Allocator, expression: String) Allocator.Error!void {
        const z = trace.zone(@src());
        defer z.end();

        const e = try strings.clone(alloc, expression);
        errdefer alloc.free(e);

        try self.watch_expressions.append(alloc, e);
    }

    /// Frees and resets all watch expressions. After this operation, self.watch_expressions
    /// will have zero items and will be ready to use.
    fn clearAndFreeWatchExpressions(self: *Self, alloc: Allocator) void {
        const z = trace.zone(@src());
        defer z.end();

        for (self.watch_expressions.items) |e| alloc.free(e);
        self.watch_expressions.clearAndFree(alloc);
    }
};

/// Holds all data pertaining to the run of a single subordinate process (though that process
/// may have more than one thread, and thus more than one PID). This data lives for the lifetime
/// of the subordinate.
const Subordinate = struct {
    const Self = @This();

    /// The subordinate process. This is essentially a full fork of the zig stdlib for subprocess
    /// management, but modified to support calling PTRACE_TRACEME before we exec().
    child: Child,

    /// If the subordinate is a PIE, this is the starting address of the itss virtual
    /// address space. We add this number to all the breakpoints we set since our
    /// Breakpoint objects store their address using only what is present in the debug
    /// symbols and doesn't take in to account the state of the running process. This
    /// gets a new value every time we launch the subordinate.
    load_addr: types.Address = types.Address.from(0),

    threads: ArrayListUnmanaged(types.PID) = .{},
    thread_breakpoints: ArrayListUnmanaged(types.ThreadBreakpoint) = .{},

    /// Stores only the data for `paused`
    paused_arena: ArenaAllocator,
    /// Describes the state of the subordinate when it has stopped after a trap
    paused: ?types.PauseData = null,

    can_use_frame_pointer_stack_unwinding: bool = false,
    has_checked_for_frame_pointer_stack_unwinding: bool = false,

    fn init(alloc: Allocator, child: Child) Self {
        return .{
            .child = child,
            .paused_arena = ArenaAllocator.init(alloc),
        };
    }

    fn clearAndFreePauseData(self: *Self) void {
        _ = self.paused_arena.reset(.free_all);
        self.paused = null;
    }
};

/// Used to look up a function declaration in a CompileUnit
const FunctionDeclIndex = struct {
    compile_unit_ndx: types.CompileUnitNdx,
    function_ndx: types.FunctionNdx,
};

/// Stores all data needed to run the debugger. A lock on this mutex is required
/// to access data in this struct. We use one lock between these fields because
/// their data is highly coupled, and we require that operations on that data to
/// be atomic, or we quickly run in to bugs.
const Data = struct {
    const Self = @This();

    /// This lock is required to be acquired to access any of the fields in this struct
    mu: Mutex = .{},

    /// Stores data that lasts longer than a single run of a subordinate process
    state: State,

    /// Stores only the data for `target`
    target_arena: ArenaAllocator,
    /// Contains the static list of debug symbols loaded from the binary we're going to debug
    target: ?*const types.Target = null,

    /// Stores only the data for `subordinate`
    subordinate_arena: ArenaAllocator,
    /// Stores data on the currently active child process, if any
    subordinate: ?Subordinate = null,

    fn init(tsa: *ThreadSafeAllocator) !Self {
        return Self{
            .state = try State.init(tsa.allocator()),
            .target_arena = ArenaAllocator.init(tsa.allocator()),
            .subordinate_arena = ArenaAllocator.init(tsa.allocator()),
        };
    }

    fn deinit(self: *Self, alloc: Allocator) void {
        if (self.subordinate) |*sub| {
            sub.clearAndFreePauseData();
        }

        self.subordinate_arena.deinit();
        self.target_arena.deinit();
        self.state.deinit(alloc);
    }
};

fn DebuggerType(comptime AdapterType: anytype) type {
    return struct {
        const Self = @This();

        // Is a ThreadSafeAllocator under the hood
        perm_alloc: Allocator,

        adapter: *AdapterType = undefined,

        data: Data,

        requests: Queue(proto.Request),
        responses: Queue(proto.Response),

        pub fn init(thread_safe_alloc: *ThreadSafeAllocator) !*Self {
            const q_timeout = time.ns_per_ms * 10;

            const perm_alloc = thread_safe_alloc.allocator();
            const self = try perm_alloc.create(Self);
            errdefer perm_alloc.destroy(self);

            self.* = .{
                .perm_alloc = thread_safe_alloc.allocator(),
                .data = try Data.init(thread_safe_alloc),
                .requests = Queue(proto.Request).init(
                    thread_safe_alloc,
                    .{ .timeout_ns = q_timeout },
                ),
                .responses = Queue(proto.Response).init(
                    thread_safe_alloc,
                    .{ .timeout_ns = q_timeout },
                ),
            };

            self.adapter = try AdapterType.init(thread_safe_alloc, &self.requests);
            return self;
        }

        pub fn deinit(self: *Self) void {
            self.data.state.shutting_down.store(true, .seq_cst);

            self.forceKillSubordinate();

            // send a poison pill to the serveRequests loop, even
            // if other callers have already sent a QuitRequest
            self.enqueue(proto.QuitRequest{});

            // wait for all background threads to shutdown cleanly
            self.data.state.shutdown_wg.wait();

            self.adapter.deinit();
            self.perm_alloc.destroy(self.adapter);

            // don't unlock becuase we free the mutex at the end
            // of the function and we're quitting the program
            self.data.mu.lock();

            self.data.state.strings.deinit(self.perm_alloc);

            // free all responses in the queue that need it
            for (self.responses.queue.items) |resp| {
                switch (resp) {
                    .received_text_output => |r| self.responses.alloc.free(r.text),
                    else => {}, // nothing to free
                }
            }

            self.requests.deinit();
            self.responses.deinit();

            self.data.deinit(self.perm_alloc);

            const alloc = self.perm_alloc;
            alloc.destroy(self);
        }

        pub fn shuttingDown(self: *Self) bool {
            const z = trace.zone(@src());
            defer z.end();

            return self.data.state.shutting_down.load(.seq_cst);
        }

        /// Spawns a background thread and handles requests on an infinite
        /// loop asynchronously. This is meant to be called on its own thread.
        pub fn serveRequestsForever(self: *Self) !Thread {
            self.data.state.shutdown_wg.start();
            const thread = try Thread.spawn(.{}, Debugger.serveRequests, .{self});
            safe.setThreadName(thread, "serveRequests");
            return thread;
        }

        fn serveRequests(self: *Self) void {
            trace.initThread();
            defer trace.deinitThread();

            const z = trace.zone(@src());
            defer z.end();

            defer self.data.state.shutdown_wg.finish();

            self.adapter.controller_thread_id.store(Thread.getCurrentId(), .seq_cst);

            var scratch_arena = ArenaAllocator.init(self.perm_alloc);
            defer scratch_arena.deinit();
            const scratch = scratch_arena.allocator();

            var done = false;
            while (!done) {
                const request = self.requests.get() catch continue;
                logRequest("async", request);

                switch (request) {
                    .quit => {
                        self.data.state.shutting_down.store(true, .seq_cst);
                        done = true;
                    },
                    .load_symbols => |req| self.loadDebugSymbolsAsync(req) catch |err| {
                        log.errf("unable to load debug symbols: {!}", .{err});
                    },
                    .launch => |req| self.launchSubordinate(scratch, req) catch |err| {
                        log.errf("unable to launch subordinate: {!}", .{err});
                    },
                    .kill => |req| self.killSubordinate(req) catch |err| {
                        log.errf("unable to kill subordinate: {!}", .{err});
                    },
                    .update_breakpoint => |cmd| self.updateBreakpoint(scratch, cmd) catch |err| {
                        log.errf("unable to update breakpoint: {!}", .{err});
                    },
                    .toggle_breakpoint => |cmd| self.toggleBreakpoint(scratch, cmd) catch |err| {
                        log.errf("unable to toggle breakpoint: {!}", .{err});
                    },
                    .cont => {
                        self.data.mu.lock();
                        defer self.data.mu.unlock();

                        self.continueExecution(.{ .force = false }) catch |err| {
                            log.errf("unable to continue execution: {!}", .{err});
                        };
                    },
                    .step => |cmd| self.step(scratch, cmd) catch |err| {
                        log.errf("unable to perform subordinate step: {!}", .{err});
                    },
                    .stopped => |cmd| self.handleSubordinateStopped(scratch, cmd) catch |err| {
                        log.errf("error during subordinate stopped event: {!}", .{err});
                    },
                    .set_hex_window_address => |cmd| {
                        self.data.mu.lock();
                        defer self.data.mu.unlock();

                        self.data.state.hex_window_address = cmd.address;
                        self.findHexWindowContents() catch |err| {
                            log.errf("unable to find hex window contents: {!}", .{err});
                        };
                    },
                    .set_watch_expressions => |cmd| {
                        self.setWatchExpressions(scratch, cmd) catch |err| {
                            log.errf("unable to find hex window contents: {!}", .{err});
                        };
                    },
                    else => {
                        log.errf("unhandled asynchronous request: {s}", .{@tagName(request)});
                    },
                }
            }
        }

        fn logRequest(comptime loc: String, req: anytype) void {
            trace.message(@tagName(req));

            switch (req) {
                // ignore these specific requests that happen too frequently
                .get_state => log.debugf("received {s} debugger command GetStateSnapshot", .{loc}),

                inline else => |active| {
                    log.debugf("received {s} debugger command {s}: {any}", .{
                        loc,
                        @typeName(@TypeOf(active)),
                        std.json.fmt(active, .{}),
                    });
                },
            }
        }

        /// Wrapper around `enqueueRequest`.
        pub fn enqueue(self: *Self, req: anytype) void {
            self.enqueueRequest(req.req());
        }

        /// Handles a message from the client to be executed later
        pub fn enqueueRequest(self: *Self, req: proto.Request) void {
            const z = trace.zone(@src());
            defer z.end();

            trace.message(@tagName(req));

            self.requests.put(req) catch |err| {
                log.errf("unable to enqueue command {s}: {!}", .{
                    @typeName(@TypeOf(req)),
                    err,
                });
            };
        }

        /// Serves a single request synchronously. If this allocates in to a field in
        /// respT, the caller owns the returned memory.
        pub fn handleRequest(self: *Self, comptime respT: anytype, req: anytype) !respT {
            const z = trace.zone(@src());
            defer z.end();

            logRequest("sync", req.req());

            switch (req.req()) {
                .get_state => |cmd| return proto.GetStateResponse{
                    .state = try self.getStateSnapshot(cmd),
                },

                inline else => {
                    log.errf("unhandled synchronous request: {s}", .{@tagName(req.req())});
                    return error.InvalidRequest;
                },
            }
        }

        fn sendMessage(
            self: *Self,
            comptime level: proto.MessageLevel,
            comptime format: String,
            args: anytype,
        ) void {
            const z = trace.zone(@src());
            defer z.end();

            const msg = fmt.allocPrint(self.responses.alloc, format, args) catch |err| {
                log.errf("unable to allocate string for MessageResponse: {!}", .{err});
                return;
            };

            const resp = proto.MessageResponse{
                .level = level,
                .message = msg,
            };
            self.responses.put(resp.resp()) catch |err| {
                log.errf("unable to enqueue MessageResponse: {!}", .{err});
                return;
            };
        }

        fn forceKillSubordinate(self: *Self) void {
            const z = trace.zone(@src());
            defer z.end();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            self.resetSubordinateState();
        }

        fn resetSubordinateState(self: *Self) void {
            const z = trace.zone(@src());
            defer z.end();

            self.adapter.reset();

            if (self.data.subordinate == null) return;
            var sub = self.data.subordinate.?;

            sub.child.extremeKillPosix() catch {};

            sub.clearAndFreePauseData();

            _ = self.data.subordinate_arena.reset(.free_all);
            self.data.subordinate = null;
        }

        pub fn stateUpdated(self: *Self) void {
            const z = trace.zone(@src());
            defer z.end();

            const resp = proto.StateUpdatedResponse{};
            self.responses.put(resp.resp()) catch |err| {
                log.errf("unable to enqueue StateUpdatedResponse: {!}", .{err});
                return;
            };
        }

        fn sendResetResponse(self: *Self) !void {
            const z = trace.zone(@src());
            defer z.end();

            defer self.stateUpdated();

            const reset = proto.ResetResponse{};
            try self.responses.put(reset.resp());
        }

        /// Launches a new thread to load all debug symbols
        fn loadDebugSymbolsAsync(self: *Self, req: proto.LoadSymbolsRequest) !void {
            self.data.state.shutdown_wg.start();
            const thread = try Thread.spawn(.{}, loadDebugSymbolsSync, .{ self, req });
            safe.setThreadName(thread, "loadDebugSymbols");
            thread.detach();
        }

        fn loadDebugSymbolsSync(self: *Self, req: proto.LoadSymbolsRequest) void {
            trace.initThread();
            defer trace.deinitThread();

            const z = trace.zone(@src());
            defer z.end();

            const start = time.microTimestamp();
            defer {
                const end = time.microTimestamp();
                const diff: f32 = @floatFromInt(end - start);
                log.debugf("debug symbols loaded in {d:.3}ms", .{diff / 1000});
                log.flush();
            }

            var resp = proto.LoadSymbolsResponse{};
            self.loadDebugSymbols(req) catch |err| {
                log.errf("unable to load debug symbols: {}", .{err});
                resp.err = err;
            };

            // avoid a race condition on shutdown
            if (self.shuttingDown()) return;

            self.responses.put(resp.resp()) catch |err| {
                log.errf("unable to enqueue load symbols response: {!}", .{err});
            };
        }

        fn loadDebugSymbols(self: *Self, req: proto.LoadSymbolsRequest) !void {
            const z = trace.zone(@src());
            defer z.end();

            defer self.data.state.shutdown_wg.finish();
            defer self.stateUpdated();

            {
                self.data.mu.lock();
                defer self.data.mu.unlock();

                if (self.data.subordinate != null) {
                    log.warn("not reloading debug symbols because the subordinate is already running");
                    return;
                }

                self.resetSubordinateState();
            }

            errdefer {
                self.data.mu.lock();
                defer self.data.mu.unlock();
                self.resetSubordinateState();
            }

            log.debugf("loading debug symbols for file: {s}", .{req.path});

            const target = try self.adapter.loadDebugSymbols(
                self.data.target_arena.allocator(),
                req,
            );
            log.debug("loading debug symbols complete");

            {
                self.data.mu.lock();
                defer self.data.mu.unlock();
                self.data.target = target;
            }
        }

        fn launchSubordinate(
            self: *Self,
            scratch: Allocator,
            req: proto.LaunchSubordinateRequest,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            defer self.stateUpdated();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            if (self.data.subordinate != null) {
                log.warn("not launching subordinate because it is already running");
                return;
            }

            if (self.data.target == null) {
                log.err("not launching the subordinate because debug symbols have not yet been loaded");
                return;
            }

            self.resetSubordinateState();
            assert(self.data.subordinate == null);
            errdefer self.resetSubordinateState();

            // inform the GUI that we are about to start the subordinate
            self.sendResetResponse() catch |err| {
                log.errf("unable to send reset response: {!}", .{err});
                return;
            };

            //
            // Start the child process
            //

            log.debug("starting subordinate process");

            var argv = ArrayList(String).init(scratch);
            try argv.append(req.path);
            var parts = mem.splitSequence(u8, req.args, " ");
            while (parts.next()) |arg| {
                // @ROBUSTNESS (jrc): this is a band-aid and should be improved in the future
                if (arg.len > 0) try argv.append(arg);
            }

            self.data.subordinate = Subordinate.init(
                self.data.subordinate_arena.allocator(),
                Child.init(argv.items, self.perm_alloc),
            );

            self.data.subordinate.?.child.stdout_behavior = .Pipe;
            self.data.subordinate.?.child.stderr_behavior = .Pipe;
            self.data.subordinate.?.child.ptrace_traceme = true;

            try self.adapter.spawnSubordinate(&self.data.subordinate.?.child);

            assert(self.data.subordinate.?.threads.items.len == 0);
            const pid = types.PID.from(self.data.subordinate.?.child.id);
            try self.data.subordinate.?.threads.append(
                self.data.subordinate_arena.allocator(),
                pid,
            );

            if (builtin.mode == .Debug) {
                // If we are running the debugger in the debugger, we need to avoid race conditions, and this is a hack to help
                // @REF: https://stackoverflow.com/questions/2359581/calling-ptrace-inside-a-ptraced-linux-process
                //
                // @TODO (jrc): use /proc/self/status to detect if we are being traced by a debugger
                // @TODO (jrc): handle this properly with signals rather than a sleep
                Thread.sleep(20 * std.time.ns_per_ms);
            }

            {
                self.data.state.shutdown_wg.start();
                const thread = try Thread.spawn(.{}, captureOutput, .{
                    self,
                    self.data.subordinate.?.child.stdout.?.reader(),
                });
                safe.setThreadName(thread, "captureStdout");
                thread.detach();
            }

            {
                self.data.state.shutdown_wg.start();
                const thread = try Thread.spawn(.{}, captureOutput, .{
                    self,
                    self.data.subordinate.?.child.stderr.?.reader(),
                });
                safe.setThreadName(thread, "captureStderr");
                thread.detach();
            }

            if (self.data.target != null and self.data.target.?.flags.pie) {
                self.data.subordinate.?.load_addr = try Adapter.parseLoadAddressFromFile(scratch, pid);
            } else {
                assert(self.data.subordinate.?.load_addr.int() == 0);
            }

            // wait for the child to start, at which point it will send us a SIGTRAP
            const timeout_secs = if (flags.CI) 20 else 2; // Github Actions default runners are insanely slow
            try self.adapter.waitForSignalSync(pid, timeout_secs * time.ns_per_s);

            // apply all breakpoints that the user has already requested
            for (self.data.state.breakpoints.items) |*bp| {
                if (!bp.flags.active) continue;
                try self.applyBreakpointToAllThreads(scratch, bp, true);
            }

            if (req.stop_on_entry) {
                try self.handleSubordinateStoppedAlreadyLocked(scratch, .{
                    .pid = pid,
                    .exited = false,
                });
            } else {
                // let the subordinate begin execution
                try self.adapter.continueExecution(pid);
                try self.adapter.waitForSignalAsync(pid);
            }

            log.debugf("started child process with pid: {d}, load address: 0x{x}", .{
                pid,
                self.data.subordinate.?.load_addr,
            });
        }

        fn captureOutput(self: *Self, reader: anytype) void {
            trace.initThread();
            defer trace.deinitThread();

            const z = trace.zone(@src());
            defer z.end();

            defer self.data.state.shutdown_wg.finish();

            while (true) {
                var buf = [_]u8{0} ** 512;
                const n = reader.read(&buf) catch |err| {
                    log.errf("unable to capture process output: {!}", .{err});
                    continue;
                };

                // the subordinate process has terminated
                if (n == 0) return;

                // the debugger is exiting
                if (self.shuttingDown()) return;

                const alloc = self.responses.alloc;

                // move from this thread's storage to an allocator accessible by the UI thread
                const output = buf[0..n];
                const text = alloc.alloc(u8, output.len) catch |err| {
                    log.errf("unable to allocate subordinate output text: {!}", .{err});
                    continue;
                };
                @memcpy(text, output);

                trace.message("read output from subordiante");

                const resp = proto.ReceivedTextOutputResponse{ .text = text };
                self.responses.put(resp.resp()) catch |err| {
                    log.errf("unable to save process output text: {!}", .{err});
                    alloc.free(text);
                    continue;
                };
            }
        }

        fn killSubordinate(self: *Self, _: proto.KillSubordinateRequest) !void {
            // @ROBUSTNESS (jrc): there is a potential memory leak here between stopping the process
            // and resetting its self.responses buffer. We should explicitly wait for all suborindate
            // threads (i.e. captureOutput) to shut down before calling self.resetSubordinateState.
            // This leak can be reproduced by spawning a subordinate that prints as fast as it can in
            // an infinite loop.
            //
            // In general, I think the captureOutput thread could use a refactor. Perhaps it's wrong
            // to use the response queue to pass program output at all since it can be very high-volume.
            self.forceKillSubordinate();

            {
                self.data.mu.lock();
                defer self.data.mu.unlock();
                self.resetSubordinateState();
            }

            try self.sendResetResponse();
        }

        fn updateBreakpoint(
            self: *Self,
            scratch: Allocator,
            req: proto.UpdateBreakpointRequest,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            defer self.stateUpdated();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            const bp: ?types.Breakpoint = b: {
                const bp_ndx: ?usize = for (self.data.state.breakpoints.items, 0..) |bp, ndx| {
                    switch (req.loc) {
                        .bid => |bid| if (bid == bp.bid) break ndx,
                        .addr => |addr| if (addr == bp.addr) break ndx,
                        .source => |r| {
                            if (bp.source_location) |loc| {
                                if (loc.file_hash == r.file_hash and loc.line == r.line) {
                                    break ndx;
                                }
                            }
                        },
                    }
                } else null;

                if (bp_ndx) |ndx| {
                    // we remove the breakpoint while retaining the original
                    // lock to ensure the operation is atomic
                    break :b self.data.state.breakpoints.orderedRemove(ndx);
                }
                break :b null;
            };

            if (bp) |b| {
                try self.deleteBreakpoint(b);
            } else {
                try self.addBreakpoint(scratch, req);
            }

            try self.findHexWindowContents();
        }

        fn addBreakpoint(
            self: *Self,
            scratch: Allocator,
            req: proto.UpdateBreakpointRequest,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            var source_loc: ?types.SourceLocation = null;
            const base_addr = switch (req.loc) {
                .addr => |addr| addr,
                .source => |r| blk: {
                    const addr = self.addressForSourceLine(r) orelse return error.BreakpointAddrNotFound;
                    source_loc = .{
                        .file_hash = r.file_hash,
                        .line = r.line,
                        .column = 0,
                    };
                    break :blk addr;
                },
                .bid => unreachable, // the `bid` argument is only for deleting breakpoints
            };

            const bid = self.data.state.nextBreakpointID();
            var breakpoint = blk: {
                const bp = types.Breakpoint{
                    .bid = bid,
                    .addr = base_addr,
                    .source_location = source_loc,
                };

                try self.data.state.breakpoints.append(self.perm_alloc, bp);
                break :blk bp;
            };

            if (self.data.subordinate != null) {
                try self.applyBreakpointToAllThreads(scratch, &breakpoint, false);
            }

            if (breakpoint.source_location) |src_loc| {
                const f = file.getCachedFile(src_loc.file_hash);
                log.debugf("breakpoint set at address 0x{x} ({s}:{d})", .{ base_addr, f.?.name, src_loc.line });
            } else {
                log.debugf("breakpoint set at address 0x{x}", .{base_addr});
            }
        }

        fn deleteBreakpoint(self: *Self, bp: types.Breakpoint) !void {
            const z = trace.zone(@src());
            defer z.end();

            if (self.data.subordinate != null) {
                try self.unsetBreakpointInAllThreads(bp);
            }

            if (builtin.mode == .Debug) {
                for (self.data.state.breakpoints.items) |b| {
                    if (b.bid == bp.bid) {
                        assert(false); // the breakpoint should have already been removed atomically
                    }
                }
            }

            if (self.data.subordinate != null) {
                var ndx: usize = 0;
                while (ndx < self.data.subordinate.?.thread_breakpoints.items.len) {
                    const tbp = self.data.subordinate.?.thread_breakpoints.items[ndx];
                    ndx += 1;

                    if (bp.bid == tbp.bid) {
                        ndx -= 1;
                        _ = self.data.subordinate.?.thread_breakpoints.orderedRemove(ndx);
                    }
                }
            }
        }

        fn toggleBreakpoint(
            self: *Self,
            scratch: Allocator,
            req: proto.ToggleBreakpointRequest,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            defer self.stateUpdated();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            if (self.data.subordinate == null) return;

            const bp = blk: {
                for (self.data.state.breakpoints.items) |*bp| {
                    if (req.id != bp.bid) continue;

                    bp.flags.active = !bp.flags.active;
                    break :blk bp;
                }

                log.warnf("breakpoint with id {d} not found", .{req.id});
                return;
            };

            if (self.data.subordinate) |*sub| {
                if (bp.flags.active) {
                    try self.applyBreakpointToAllThreads(scratch, bp, false);
                } else {
                    try self.unsetBreakpointInAllThreads(bp.*);
                }

                // also toggle the breakpoint copy on the Paused struct if needed
                if (sub.paused) |*paused| {
                    if (paused.breakpoint) |*paused_bp| {
                        paused_bp.flags.active = !paused_bp.flags.active;
                    }
                }
            }

            // update the hex window since we might be looking at program text that was changed
            try self.findHexWindowContents();
        }

        /// Pause the subordinate if needed, set the breakpoint in the text segment of all
        /// threads, and continue execution if we paused the process
        fn applyBreakpointToAllThreads(
            self: *Self,
            scratch: Allocator,
            bp: *types.Breakpoint,
            launching_subordinate: bool,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            const sub = self.data.subordinate.?;
            const subordinate_pid = types.PID.from(sub.child.id);

            const should_continue = blk: {
                // the subordinate has been launched but is already paused before its first instruction
                if (launching_subordinate) break :blk false;

                // execution is already paused, nothing to do
                if (sub.paused != null) break :blk false;

                // the subordinate has been started and is not already paused
                try self.adapter.temporarilyPauseSubordinate(subordinate_pid);
                break :blk true;
            };

            defer if (should_continue) self.continueExecution(.{}) catch |err| {
                log.errf(
                    "unable to continue execution after setting breakpoint: {!}",
                    .{err},
                );
            };

            // apply the breakpoint in all subordinate threads
            var thread_bps = ArrayList(types.ThreadBreakpoint).init(scratch);
            for (sub.threads.items) |pid| {
                const tbp = try self.adapter.setBreakpoint(sub.load_addr, bp, pid);
                try thread_bps.append(tbp);
            }
            try self.data.subordinate.?.thread_breakpoints.appendSlice(
                self.data.subordinate_arena.allocator(),
                thread_bps.items,
            );

            // apply back to the main breakpoint list
            for (self.data.state.breakpoints.items, 0..) |*b, ndx| {
                if (b.bid != bp.bid) continue;

                self.data.state.breakpoints.items[ndx] = bp.*;
                break;
            }
        }

        /// Applies one or more "internal" breakpoints to an single child thread
        fn applyInternalBreakpoints(
            self: *Self,
            scratch: Allocator,
            pid: types.PID,
            addrs: []types.Address,
            call_frame_addr: ?types.Address,
            max_stack_frames: ?usize,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            assert(self.data.subordinate != null);
            assert(self.data.subordinate.?.paused != null);

            var bps = try ArrayList(types.Breakpoint).initCapacity(scratch, addrs.len);
            var tbps = try ArrayList(types.ThreadBreakpoint).initCapacity(scratch, addrs.len);
            for (addrs) |addr| {
                var bp = types.Breakpoint{
                    .flags = .{
                        .active = true,
                        .internal = true,
                    },
                    .bid = self.data.state.nextBreakpointID(),
                    .addr = addr,
                    .call_frame_addr = call_frame_addr,
                    .max_stack_frames = max_stack_frames,
                };

                // @TODO (jrc): add a check here to make sure that a breakpoint is not already
                // set at this address; it will allow the caller code to be much nicer

                const tbp = try self.adapter.setBreakpoint(self.data.subordinate.?.load_addr, &bp, pid);
                bps.appendAssumeCapacity(bp);
                tbps.appendAssumeCapacity(tbp);

                log.debugf("internal breakpoint set at address 0x{x}", .{addr});
            }

            try self.data.state.breakpoints.appendSlice(self.perm_alloc, bps.items);
            try self.data.subordinate.?.thread_breakpoints.appendSlice(
                self.data.subordinate_arena.allocator(),
                tbps.items,
            );
        }

        fn unsetBreakpointInAllThreads(self: *Self, bp: types.Breakpoint) !void {
            const z = trace.zone(@src());
            defer z.end();

            const sub = self.data.subordinate.?;
            const subordinate_pid = types.PID.from(sub.child.id);

            const should_continue = blk: {
                if (sub.paused != null) {
                    break :blk false;
                }

                // the subordinate has been started and is not already paused
                try self.adapter.temporarilyPauseSubordinate(subordinate_pid);
                break :blk true;
            };

            defer {
                if (should_continue) {
                    self.continueExecution(.{}) catch |err| {
                        log.errf("unable to continue execution after unsetting breakpoint: {!}", .{err});
                    };
                }
            }

            for (sub.threads.items) |pid| {
                try self.adapter.unsetBreakpoint(sub.load_addr, bp, pid);
            }
        }

        /// Deletes all internal breakpoints since these only last for the lifetime of one run of subordinate execution
        fn clearInternalBreakpoints(self: *Self, scratch: Allocator) !void {
            const z = trace.zone(@src());
            defer z.end();

            const internal_bps = blk: {
                var bps = ArrayList(types.Breakpoint).init(scratch);
                var to_remove = ArrayList(usize).init(scratch);
                for (self.data.state.breakpoints.items, 0..) |bp, ndx| {
                    if (!bp.flags.internal) continue;

                    try bps.append(bp);
                    try to_remove.append(ndx);
                }

                for (to_remove.items, 0..) |i, ndx| {
                    _ = self.data.state.breakpoints.swapRemove(i - ndx);
                }

                break :blk try bps.toOwnedSlice();
            };

            const internal_thread_breakpoints = blk: {
                var tbps = ArrayList(types.ThreadBreakpoint).init(scratch);
                var to_remove = ArrayList(usize).init(scratch);

                if (self.data.subordinate) |*sub| {
                    for (sub.thread_breakpoints.items, 0..) |tbp, ndx| {
                        for (internal_bps) |ibp| {
                            if (tbp.bid != ibp.bid) continue;

                            try tbps.append(tbp);
                            try to_remove.append(ndx);
                        }
                    }

                    for (to_remove.items, 0..) |ndx, i| {
                        _ = sub.thread_breakpoints.swapRemove(ndx - i);
                    }
                }

                break :blk try tbps.toOwnedSlice();
            };

            log.debugf("unsetting {d} internal breakpoints", .{internal_bps.len});

            // the program has shutdown, there are no more breakpoints to unset
            if (self.data.subordinate == null) return;

            for (internal_bps) |bp| {
                for (internal_thread_breakpoints) |tbp| {
                    if (bp.bid != tbp.bid) continue;

                    try self.adapter.unsetBreakpoint(self.data.subordinate.?.load_addr, bp, tbp.pid);
                }
            }
        }

        const ContinueExecutionOpts = packed struct {
            force: bool = true,
            step_over: bool = false,
        };

        fn continueExecution(self: *Self, opts: ContinueExecutionOpts) !void {
            const z = trace.zone(@src());
            defer z.end();

            //
            // @SEARCH: CONTEXE
            //
            // If we are paused at a breakpoint that we set ourselves (i.e. we trapped because
            // of an interrupt instruction we set rather than just any old signal), we immediately
            // back the PC up by one and reset the instruction byte so that way the register view
            // shows a consistent view of the world to the user. It makes it appear as though we
            // actually did pause just before the address at the breakpoint was executed.
            //
            // Then, later, when we want to continue execution, we single-step to ensure we actually
            // execute the instruction, reset the instruction to the interrupt instruction, and
            // then continue execution.
            //

            if (self.data.subordinate == null) {
                log.warn("cannot continue execution: subordinate is not running");
                return;
            }

            defer self.stateUpdated();

            const pid = types.PID.from(self.data.subordinate.?.child.id);
            const load_addr = self.data.subordinate.?.load_addr;

            if (self.data.subordinate.?.paused) |paused| done: {
                if (paused.breakpoint) |bp| {
                    const exists = e: {
                        for (self.data.state.breakpoints.items) |b| {
                            if (b.bid == bp.bid) break :e true;
                        }
                        break :e false;
                    };

                    if (!opts.step_over and (!exists or !bp.flags.active)) {
                        // we only need to re-apply the breakpoint instruction if the
                        // bp is still active (it might be the case that while we were
                        // stopped at this breakpoint, the bp was disabled or deleted
                        // by the user)
                        break :done;
                    }

                    if (builtin.mode == .Debug) {
                        const regs = try self.adapter.getRegisters(pid);
                        assert(regs.pc().eql(load_addr.add(bp.addr)));
                    }

                    var buf = [_]u8{0};
                    try self.adapter.peekData(pid, load_addr, bp.addr, &buf);

                    var instruction = [_]u8{bp.instruction_byte};
                    if (buf[0] == arch.InterruptInstruction) {
                        try self.adapter.pokeData(pid, load_addr, bp.addr, &instruction);
                    }

                    try self.adapter.singleStepAndWait(pid);

                    // we still want to reset to the interrupt instruction in the case that
                    // we're stepping out of a recursive function
                    if (!bp.flags.internal or bp.max_stack_frames != null) {
                        instruction[0] = arch.InterruptInstruction;
                        try self.adapter.pokeData(pid, load_addr, bp.addr, &instruction);
                    }
                }
            } else if (!opts.force) {
                // the user themselves has requested continuing execution even though
                // we're not stopped at a breakpoint, so we should ignore their request
                log.warn("cannot continue execution because the subordinate is already running");
                return;
            }

            self.data.subordinate.?.clearAndFreePauseData();

            try self.adapter.continueExecution(pid);
            try self.adapter.waitForSignalAsync(pid);
        }

        const BreakpointAndPID = struct {
            bp: ?types.Breakpoint,
            pid: types.PID,
        };

        fn step(self: *Self, scratch: Allocator, req: proto.StepRequest) !void {
            const z = trace.zone(@src());
            defer z.end();

            // @NOTE (jrc): All step operations must be atomic,
            // which is why we take a giant critical section
            self.data.mu.lock();
            defer self.data.mu.unlock();

            if (self.data.subordinate == null) {
                log.warn("cannot step: subordinate is not running");
                return;
            }

            const bpp = blk: {
                if (self.data.subordinate.?.paused) |paused| {
                    if (paused.breakpoint) |bp| {
                        // unset the breakpoint address before stepping
                        try self.adapter.pokeData(
                            paused.pid,
                            self.data.subordinate.?.load_addr,
                            bp.addr,
                            &[_]u8{bp.instruction_byte},
                        );
                    }

                    break :blk BreakpointAndPID{
                        .bp = paused.breakpoint,
                        .pid = paused.pid,
                    };
                }

                log.warn("not stepping because the subordinate is not paused");
                return;
            };

            switch (req.step_type) {
                .single => try self.singleStep(bpp),
                .into => try self.stepInto(scratch, bpp),
                .out_of => try self.stepOutOf(scratch, bpp),
                .over => try self.stepOver(scratch, bpp),
            }
        }

        fn singleStep(self: *Self, bpp: BreakpointAndPID) !void {
            const z = trace.zone(@src());
            defer z.end();

            try self.adapter.singleStep(bpp.pid);
        }

        fn stepInto(self: *Self, scratch: Allocator, bpp: BreakpointAndPID) !void {
            const z = trace.zone(@src());
            defer z.end();

            //
            // @SEARCH: STEPINTO
            //
            // Single step zero or many times, up to a limit. Each time, check whether
            // we've hit a known line of source code. If yes, we're done. If we don't
            // ever hit a known line of source code, perform a step next operation as
            // if we were still stopped in the function in which we started.
            //

            const start_regs = try self.adapter.getRegisters(bpp.pid);
            const start_pc = start_regs.pc();
            const load_addr = self.data.subordinate.?.load_addr;

            var found_line_of_code = false;
            defer {
                if (!found_line_of_code) {
                    // We did not find a known line of code, so pretend as though we just did a step
                    // next in the function in which we started. We have to do this in the defer because
                    // we have to reset the breakpoint back to its original value before calling
                    // step next, since step next continues execution.
                    self.stepOverForPC(scratch, bpp, start_pc) catch |err| {
                        log.errf("unable to perform subordinate step: {!}", .{err});
                    };
                }
            }

            // unset a breakpoint if we're stopped at one
            if (bpp.bp) |bp| {
                if (!bp.flags.internal) try self.unsetBreakpointInAllThreads(bp);
            }

            // reset the breakpoint, if any
            defer {
                if (bpp.bp) |bp| {
                    if (!bp.flags.internal) {
                        // find and mutate the original breakpoint
                        const src_bp = blk: {
                            for (self.data.state.breakpoints.items) |*b| {
                                if (b.bid.neq(bp.bid)) continue;
                                break :blk b;
                            }
                            unreachable;
                        };

                        self.applyBreakpointToAllThreads(scratch, src_bp, false) catch |err| {
                            log.errf("unable to re-apply breakpoint after step into: {!}", .{err});
                        };
                    }
                }
            }

            // we're not at a call instruction, so we should repeatedly single-step until
            // we hit a known line of code, or we hit the limit on the number of times we're
            // willing to single step
            const start_src = self.sourceForAddress(start_regs.pc());
            for (0..64) |_| {
                try self.adapter.singleStepAndWait(bpp.pid);

                const regs = try self.adapter.getRegisters(bpp.pid);
                if (self.sourceForAddress(regs.pc())) |current_src| {
                    if (start_src == null or !start_src.?.loc.eql(current_src.loc)) {
                        // @NOTE (jrc): some compilers (i.e. certain gcc versions) emit line info
                        // that leaves us on the very first line of the new function we're stepping
                        // in to when we have base pointers enabled. In this case, the instruction
                        // we're stopped at is the instruction that pushes the base pointer. We want
                        // to step past this line if that's the case (i.e. step past the line that
                        // is just the function signature, and wind up on the first line in the body
                        // of the function).
                        if (self.data.subordinate) |sub| {
                            if (sub.can_use_frame_pointer_stack_unwinding) {
                                var instr_buf = [_]u8{0};
                                try self.adapter.peekData(bpp.pid, load_addr, regs.pc(), &instr_buf);
                                if (instr_buf[0] == arch.PushFramePointerInstruction) {
                                    // run to the next instruction
                                    continue;
                                }
                            }
                        }

                        // we've hit the next line of code
                        found_line_of_code = true;
                        break;
                    }
                }
            }

            if (found_line_of_code) {
                // if we stopped on a line that has a breakpoint, toggle its instruction back to its
                // original value so we're prepared for future operations (i.e. a continue request)
                const regs = try self.adapter.getRegisters(bpp.pid);
                for (self.data.state.breakpoints.items) |*bp| {
                    if (!bp.flags.active or bp.flags.internal or (bp.addr.add(load_addr).neq(regs.pc()))) {
                        continue;
                    }

                    var instruction = [_]u8{bp.instruction_byte};
                    try self.adapter.pokeData(bpp.pid, load_addr, bp.addr, &instruction);
                }

                const req = proto.SubordinateStoppedRequest{ .pid = bpp.pid, .exited = false };
                try self.handleSubordinateStoppedAlreadyLocked(scratch, req);
                return;
            }
        }

        fn stepOutOf(self: *Self, scratch: Allocator, bpp: BreakpointAndPID) !void {
            const z = trace.zone(@src());
            defer z.end();

            //
            // @SEARCH: STEPOUT
            //
            // Set a breakpoint at the caller's base frame address, then continue until it is hit.
            // Note that in the case of recursive functions, this is not sufficient because we may
            // be at depth 2, but may be recursing until depth 10, which means that the first time
            // we hit the breakpoint at the base frame address, we will be at max depth. So, when
            // we hit those breakpoints, we repeatedly ignore them until we're at the correct depth,
            // or we hit some other breakpoint along the way.
            //

            const frames = self.data.subordinate.?.paused.?.stack_frames;
            if (frames.len > 1) {
                var addrs = [_]types.Address{frames[1].address};
                const max_stack_frames = frames.len - 1;
                try self.applyInternalBreakpoints(
                    scratch,
                    bpp.pid,
                    &addrs,
                    null,
                    max_stack_frames,
                );
            }

            try self.continueExecution(.{});
        }

        fn stepOver(self: *Self, scratch: Allocator, bpp: BreakpointAndPID) !void {
            const regs = try self.adapter.getRegisters(bpp.pid);
            try self.stepOverForPC(scratch, bpp, regs.pc());
        }

        fn stepOverForPC(self: *Self, scratch: Allocator, bpp: BreakpointAndPID, pc: types.Address) !void {
            const z = trace.zone(@src());
            defer z.end();

            //
            // @SEARCH: STEPOVER
            // @REF: https://youtu.be/IKnTr7Zms1k?si=WbeW3KT3BT6kQyhV&t=1936
            //
            // Step over (also called "step next") is implemented as follows:
            // 1. Set an internal breakpoint on every line of the function on this thread.
            //    Don't internal set breakpoints on lines that already have user-specified
            //    breakpoints or statements that come from inlined function calls.
            // 2. Set an internal breakpoint on the return address on this thread if one
            //    is not already set in case the function returns to the caller
            // 3. Set an internal breakpoint on this thread on the last defer function if
            //    one is not already set and the language has deferred functions
            //    (What is the point of this one? Re-watch the talk above and figure out
            //     why this comment is here.)
            // 4. Reset the breakpoint instruction and continue execution
            //
            // Then, the next time a breakpoint is hit, if it belongs to this thread,
            // ensure that our stack frame has not changed. This stack frame check is
            // required in the case of recursive functions. If the breakpoint we hit
            // doesn't belong to this thread, clear all internal thread breakpoints
            // because we have changed out execution context.
            //

            // (1)
            const current_func = self.functionAtAddr(pc) orelse {
                log.warn("cannot step to next line of code because current function is unknown");
                return;
            };

            const existing_applied_thread_breakpoints = blk: {
                var tbps = ArrayList(types.ThreadBreakpoint).init(scratch);
                for (self.data.subordinate.?.thread_breakpoints.items) |tbp| {
                    if (tbp.pid == bpp.pid and tbp.flags.is_applied) try tbps.append(tbp);
                }
                break :blk try tbps.toOwnedSlice();
            };

            const existing_applied_breakpoints = blk: {
                var bps = ArrayList(types.Breakpoint).init(scratch);
                for (self.data.state.breakpoints.items) |bp| {
                    assert(!bp.flags.internal);
                    for (existing_applied_thread_breakpoints) |tbp| {
                        if (bp.bid != tbp.bid) continue;

                        assert(bp.flags.active);
                        try bps.append(bp);
                    }
                }
                break :blk try bps.toOwnedSlice();
            };

            const internal_bp_addrs_to_set = blk: {
                const cu = self.data.target.?.compile_units[current_func.cu_ndx.int()];
                var addrs = ArrayList(types.Address).init(scratch);

                for (current_func.func.statements) |stmt| next: {
                    // don't set a breakpoint on the line we're already stopped at
                    if (stmt.breakpoint_addr == pc) continue;

                    // don't set breakpoints in function bodies that have been inlined
                    for (current_func.func.inlined_function_indices) |inlined_ndx| {
                        // @QUESTION (jrc): Do we want to set a breakpoint at the inlined function's
                        // callsite source location? How would we do that?
                        if (try shouldSkipInlinedFunctionStatement(&cu, &stmt, inlined_ndx)) {
                            break :next;
                        }
                    }

                    for (current_func.func.addr_ranges) |addr_range| {
                        if (addr_range.contains(stmt.breakpoint_addr)) {
                            const exists = e: {
                                for (existing_applied_breakpoints) |bp| {
                                    if (bp.addr == stmt.breakpoint_addr) {
                                        break :e true;
                                    }
                                }
                                break :e false;
                            };

                            if (!exists) {
                                try addrs.append(stmt.breakpoint_addr);
                                break :next;
                            }
                        }
                    }
                }

                break :blk try addrs.toOwnedSlice();
            };

            const paused = self.data.subordinate.?.paused.?;
            try self.applyInternalBreakpoints(
                scratch,
                bpp.pid,
                internal_bp_addrs_to_set,
                paused.frame_base_addr,
                null,
            );

            // (2)
            if (paused.stack_frames.len >= 2) {
                const return_addr = paused.stack_frames[1].address;
                const exists = e: {
                    for (existing_applied_breakpoints) |bp| {
                        if (bp.addr == return_addr) break :e true;
                    }
                    break :e false;
                };

                if (!exists) {
                    var return_addrs = [_]types.Address{return_addr};
                    try self.applyInternalBreakpoints(
                        scratch,
                        bpp.pid,
                        @ptrCast(&return_addrs),
                        null,
                        null,
                    );
                }
            }

            // (3) @TODO (jrc): implement this (?)

            // (4)
            if (bpp.bp) |bp| {
                self.adapter.pokeData(
                    bpp.pid,
                    self.data.subordinate.?.load_addr,
                    bp.addr,
                    &[_]u8{arch.InterruptInstruction},
                ) catch |err| {
                    log.errf("unable to reset breakpoint instruction: {!}", .{err});
                };
            }

            try self.continueExecution(.{ .step_over = true });
        }

        /// Determines whether or not the types.InlinedFunctionDecl at inlined_ndx in the current
        /// CU's symbol table contains the given source statement, or if any of the inlined functions
        /// within this function contain the statement.
        ///
        /// @QUESTION (jrc): can we make this iterative rather than recursive?
        fn shouldSkipInlinedFunctionStatement(
            _: *const types.CompileUnit,
            _: *const types.SourceStatement,
            _: types.FunctionNdx,
        ) !bool {
            const z = trace.zone(@src());
            defer z.end();

            // @TODO (jrc): This needs to be re-implemented. We don't parse
            // inlined functions from DWARF correctly yet, so we need to
            // do that first.

            return false;

            // const inline_sym = try cu.symbolAt(inlined_ndx);
            // const inlined_func = switch (inline_sym) {
            //     .inlined_function => |f| f,
            //     else => {
            //         log.errf("symbol at ndx {d} is not an inlined function (got {s})", .{
            //             inlined_ndx,
            //             @tagName(inline_sym),
            //         });
            //         return error.InvalidInlineFunctionSymbolNdx;
            //     },
            // };

            // for (inlined_func.addr_ranges) |inlined_range| {
            //     if (inlined_range.contains(stmt.breakpoint_addr)) {
            //         return true;
            //     }
            // }

            // for (inlined_func.inlined_functions.items) |sub_inlined_ndx| {
            //     if (try shouldSkipInlinedFunctionStatement(cu, stmt, sub_inlined_ndx)) {
            //         return true;
            //     }
            // }

            // return false;
        }

        fn addressForSourceLine(self: *Self, loc: types.SourceLocation) ?types.Address {
            const z = trace.zone(@src());
            defer z.end();

            if (self.data.target) |target| {
                for (target.compile_units) |cu| {
                    var addr: ?types.Address = null;
                    for (cu.sources) |src| {
                        if (src.file_hash != loc.file_hash) continue;

                        for (src.statements) |stmt| {
                            if (stmt.line != loc.line) continue;

                            // @NOTE (jrc): we look up the _last_ known breakpoint because there are many entries
                            // per deferred line of code in Zig, and we usually want the last one. I'm not sure if
                            // this is intentional or not in Zig though because all those line entires only occur
                            // in debug builds. I think that we should actually be using all those addresses, but
                            // types.Breakpoint only has a single address, and it's really nice that it doesn't need
                            // any allocations, so it makes serialization much easier, so I'm hesitant to add a
                            // slice to it. Needs more investigation.
                            addr = stmt.breakpoint_addr;
                            if (cu.language != .Zig) return addr;
                        }
                    }

                    if (addr) |a| return a;
                }
            }

            return null;
        }

        /// Returns the function declaration and the index of its CompileUnit for the given.
        /// address. `addr` should NOT have the load adderss applied; it will be applied
        /// automatically by this function.
        fn functionAtAddr(self: *Self, addr: types.Address) ?struct {
            cu_ndx: types.CompileUnitNdx,
            func: types.Function,
        } {
            const z = trace.zone(@src());
            defer z.end();

            if (self.data.subordinate) |sub| {
                const target_addr = addr.sub(sub.load_addr);
                for (self.data.target.?.compile_units, 0..) |cu, cu_ndx| {
                    if (cu.functions.findForAddress(target_addr)) |func| {
                        return .{
                            .cu_ndx = types.CompileUnitNdx.from(cu_ndx),
                            .func = func,
                        };
                    }
                }
            }

            return null;
        }

        /// Returns the source statement, source location, and the index of its CompileUnit
        /// for the given address. `addr` should NOT have the load adderss applied;
        /// it will be applied automatically by this function.
        fn sourceForAddress(self: *Self, addr: types.Address) ?struct {
            cu_ndx: types.CompileUnitNdx,
            stmt: types.SourceStatement,
            loc: types.SourceLocation,
        } {
            const z = trace.zone(@src());
            defer z.end();

            // cannot look up source for address because we do not yet know the load address
            if (self.data.subordinate == null) return null;

            const func = blk: {
                if (self.functionAtAddr(addr)) |f| break :blk f;
                return null;
            };

            if (func.func.source_loc == null) return null;

            const cu = self.data.target.?.compile_units[func.cu_ndx.int()];
            for (cu.sources) |src| {
                if (src.file_hash != func.func.source_loc.?.file_hash) continue;

                for (src.statements) |stmt| {
                    const stmt_addr = self.data.subordinate.?.load_addr.add(stmt.breakpoint_addr);
                    if (addr.eql(stmt_addr)) {
                        return .{
                            .cu_ndx = func.cu_ndx,
                            .stmt = stmt,
                            .loc = types.SourceLocation{
                                .file_hash = src.file_hash,
                                .line = stmt.line,
                                .column = 0,
                            },
                        };
                    }
                }
            }

            return null;
        }

        /// All allocations will be made using the Allocator on the req, and the caller owns returned memory. The allocator
        /// passed via req must be an arena allocator.
        fn getStateSnapshot(self: *Self, req: proto.GetStateRequest) !types.StateSnapshot {
            const z = trace.zone(@src());
            defer z.end();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            //
            // Copy all non-internal breakpoints
            //

            var breakpoints = ArrayList(types.Breakpoint).init(req.alloc);
            errdefer breakpoints.deinit();

            for (self.data.state.breakpoints.items) |bp| {
                if (bp.flags.internal) continue;
                try breakpoints.append(bp);
            }
            std.sort.block(types.Breakpoint, breakpoints.items, {}, types.Breakpoint.sort);

            //
            // Copy all PausedData if the subordinate is stopped
            //

            const paused: ?types.PauseData = blk: {
                if (self.data.subordinate) |sub| {
                    if (sub.paused) |paused| {
                        // perform a full copy to the target allocator
                        break :blk try paused.copy(req.alloc);
                    }
                }

                // the subordinate is either not started, or actively running (not paused)
                break :blk null;
            };
            errdefer if (paused) |p| p.deinit(req.alloc);

            return types.StateSnapshot{
                .breakpoints = try breakpoints.toOwnedSlice(),
                .paused = paused,
            };
        }

        fn handleSubordinateStopped(self: *Self, scratch: Allocator, req: proto.SubordinateStoppedRequest) !void {
            const z = trace.zone(@src());
            defer z.end();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            try self.handleSubordinateStoppedAlreadyLocked(scratch, req);
        }

        fn handleSubordinateStoppedAlreadyLocked(self: *Self, scratch: Allocator, req: proto.SubordinateStoppedRequest) !void {
            defer self.stateUpdated();

            // the subordinate is reporting that it has shut down
            if (req.exited or self.data.subordinate == null) {
                self.resetSubordinateState();
                try self.clearInternalBreakpoints(scratch);
                return;
            }

            // we received a signal that we don't care about, so don't actually pause the debugger
            if (!req.should_stop_debugger) {
                try self.continueExecution(.{});
                return;
            }

            // we're no longer stopped (edge case)
            var registers = try self.adapter.getRegisters(req.pid);
            if (registers.pc().int() == 0) return;

            const sub = self.data.subordinate.?;
            const str_cache = try strings.Cache.init(scratch);

            //
            // @SEARCH: STACKRBP
            //
            // First, compute the backtrace for the PC at which the subordinate is stopped
            // using DWARF-based unwinding. Then, compute the stack trace using the frame
            // pointer. Compare the results to see if using rbp is possible since it's faster
            // and more reliable.
            //

            const unwind_res: types.UnwindResult = blk: {
                // unwind with the frame pointer and return if we know we can
                if (self.data.subordinate.?.can_use_frame_pointer_stack_unwinding) {
                    log.info("unwinding with base pointer");
                    break :blk try self.unwindStackWithFramePointer(scratch, req.pid, &registers);
                }

                // unwind using the platform-specific unwinder
                const unwinder = self.data.target.?.unwinder.findForAddr(registers.pc().sub(sub.load_addr));
                if (unwinder == null) {
                    log.err("unable to unwind call stack: unwinder not found");
                    break :blk types.UnwindResult{
                        .call_stack_addrs = &.{},
                        .frame_base_addr = types.Address.from(0),
                    };
                }
                const unwind = try self.adapter.unwindStack(
                    scratch,
                    req.pid,
                    sub.load_addr,
                    &registers,
                    self.data.target.?.addr_size,
                    unwinder.?,
                    null,
                );

                // if we know we cannot use frame pointers to unwind the stack, we're done
                if (self.data.subordinate.?.has_checked_for_frame_pointer_stack_unwinding) {
                    log.info("unwinding with dwarf");
                    break :blk unwind;
                }
                self.data.subordinate.?.has_checked_for_frame_pointer_stack_unwinding = true;

                // attempt to unwind the stack with the frame pointer
                const bp_unwind = self.unwindStackWithFramePointer(scratch, req.pid, &registers) catch |err| {
                    log.errf("unable to unwind the stack unwing base pointers: {!}", .{err});
                    break :blk unwind;
                };

                // check to see if we have a match between frame-based unwinding and platform-specific unwinding
                if (unwind.frame_base_addr.eql(bp_unwind.frame_base_addr)) {
                    self.data.subordinate.?.can_use_frame_pointer_stack_unwinding = true;
                    break :blk bp_unwind;
                }

                break :blk unwind;
            };

            //
            // Find the breakpoint we've hit, if any
            //

            var recursion_satisfied = true;
            const breakpoint: ?types.Breakpoint = blk: {
                for (self.data.state.breakpoints.items) |*bp| {
                    if (!bp.flags.active) continue;

                    // are we at the breakpoint's address?
                    if (bp.addr.int() != registers.pc().sub(sub.load_addr).int() - 1) continue;

                    // if specified, are we at the breakpoint's call frame?
                    if (bp.call_frame_addr) |cfa| {
                        recursion_satisfied = cfa == unwind_res.frame_base_addr;
                    } else {
                        // if specified, do we have the correct number of stack
                        // frames in a step out operation?
                        if (bp.max_stack_frames) |max_frames| {
                            recursion_satisfied = max_frames == unwind_res.call_stack_addrs.len;
                        }
                    }

                    // @SEE: CONTEXE
                    registers.setPC(registers.pc().sub(types.Address.from(1)));
                    try self.adapter.setRegisters(req.pid, &registers);
                    if (unwind_res.call_stack_addrs.len > 0) {
                        // subtrace one from the PC
                        unwind_res.call_stack_addrs[0] = unwind_res.call_stack_addrs[0].sub(types.Address.from(1));
                    }

                    bp.hit_count += 1;

                    log.debugf("stopped at bp: {any}", .{bp});
                    break :blk bp.*;
                }

                log.debug("stopped at null breakpoint");
                break :blk null;
            };

            //
            // Render stack frame info and detect local variables
            //

            errdefer self.data.subordinate.?.clearAndFreePauseData();
            const paused_alloc = self.data.subordinate.?.paused_arena.allocator();

            const stack_frames = try paused_alloc.alloc(
                types.StackFrame,
                unwind_res.call_stack_addrs.len,
            );

            var local_variables = ArrayList(String).init(paused_alloc);

            for (unwind_res.call_stack_addrs, 0..) |addr, ndx| {
                const name: String = blk: {
                    if (addr.eqlInt(0)) break :blk types.Unknown;

                    const func = self.functionAtAddr(addr);
                    if (func) |fun| {
                        if (ndx == 0) {
                            // store the variable identifiers that are local to this function
                            const stopped_at_addr = registers.pc().sub(sub.load_addr);
                            if (self.data.target.?.compileUnitForAddr(stopped_at_addr)) |cu| {
                                for (fun.func.variables) |var_ndx| {
                                    if (var_ndx.int() >= cu.variables.len) {
                                        log.errf("variable index out of range (got {d}, len {d})", .{
                                            var_ndx,
                                            cu.variables.len,
                                        });
                                        continue;
                                    }

                                    const variable = cu.variables[var_ndx.int()];
                                    const var_name = try self.data.target.?.strings.getOwned(scratch, variable.name);
                                    if (var_name) |local| {
                                        try local_variables.append(local);
                                    } else {
                                        log.err("variable name not found");
                                    }
                                }
                            }
                        }

                        break :blk self.data.target.?.strings.get(fun.func.name) orelse types.Unknown;
                    }

                    break :blk types.Unknown;
                };

                const name_hash = try str_cache.add(name);
                stack_frames[ndx] = .{
                    .address = addr,
                    .name = name_hash,
                };
            }

            //
            // Find the source line we're stopped at, if any
            //

            // @CLEANUP (jrc): the logging here is a bit tedious
            const source_loc = self.sourceForAddress(registers.pc());
            const pc_minus_load_addr = registers.pc().sub(self.data.subordinate.?.load_addr);
            if (source_loc) |src| {
                if (file.getCachedFile(src.loc.file_hash)) |f| {
                    log.debugf("stopped at pc: 0x{x} (0x{x}), source location: {s}:{d}", .{ registers.pc(), pc_minus_load_addr, f.name, src.loc.line });
                } else {
                    log.debugf("stopped at pc: 0x{x} (0x{x}), source location: {any}", .{ registers.pc(), pc_minus_load_addr, src });
                }
            } else {
                log.debugf("stopped at pc: 0x{x} (0x{x}), source location: (unknown)", .{ registers.pc(), pc_minus_load_addr });
            }

            //
            // Accumulate result data and return
            //

            self.data.subordinate.?.paused = .{
                .pid = req.pid,
                .registers = registers,
                .source_location = if (source_loc) |loc| loc.loc else null,
                .breakpoint = breakpoint,
                .frame_base_addr = unwind_res.frame_base_addr,
                .stack_frames = stack_frames,
                .hex_displays = &.{},
                .locals = &.{},
                .watches = &.{},
                .strings = str_cache,
            };

            // no need to run these heavy calculations if out call frame address
            // doesn't match what was requested by the breakpoint
            if (!recursion_satisfied) {
                self.enqueue(proto.ContinueRequest{});
            } else {
                try self.clearInternalBreakpoints(scratch);

                try self.calculateWatchExpressions(scratch);
                try self.calculateLocalVariables(scratch, try local_variables.toOwnedSlice());
                try self.findHexWindowContents();
            }
        }

        /// Caller owns returned memory
        fn unwindStackWithFramePointer(
            self: *Self,
            scratch: Allocator,
            pid: types.PID,
            registers: *const arch.Registers,
        ) !types.UnwindResult {
            const z = trace.zone(@src());
            defer z.end();

            const endianness = builtin.cpu.arch.endian();
            const data_size = self.data.target.?.addr_size.bytes();

            const sub = self.data.subordinate.?;

            var frame_addr = registers.bp();
            var return_addr = registers.pc();

            var stack = ArrayList(types.Address).init(scratch);
            errdefer stack.deinit();
            try stack.append(return_addr);

            const max = math.pow(usize, 2, 12);
            for (0..max) |ndx| {
                const buf = try scratch.alloc(u8, data_size);

                // find the previous frame's PC
                try self.adapter.peekData(
                    pid,
                    sub.load_addr,
                    frame_addr.add(types.Address.from(data_size)),
                    buf,
                );
                return_addr = types.Address.from(mem.readInt(u64, @ptrCast(buf), endianness));
                try stack.append(return_addr);

                // find the address of the next stack frame to search for
                try self.adapter.peekData(pid, sub.load_addr, frame_addr, buf);
                frame_addr = types.Address.from(mem.readInt(u64, @ptrCast(buf), endianness));

                // we're at the top of the stack
                if (frame_addr.int() == 0) break;

                assert(ndx < max - 1);
            }

            return types.UnwindResult{
                .call_stack_addrs = try stack.toOwnedSlice(),

                // +8 once to get the current functions return address (the CFA), then
                // +8 again to get the previous functions's base pointer (the frame base)
                // @SRC: https://stackoverflow.com/questions/66699927/find-frame-base-and-variable-locations-using-dwarf-version-4
                .frame_base_addr = registers.bp().add(types.Address.from(8 + 8)),
            };
        }

        /// Looks up and stores the data the user would like displayed in the memory hex viewer window
        fn findHexWindowContents(self: *Self) !void {
            const z = trace.zone(@src());
            defer z.end();

            defer self.stateUpdated();

            if (self.data.state.hex_window_address == null or
                self.data.subordinate == null or
                self.data.subordinate.?.paused == null)
            {
                // nothing to do
                return;
            }

            const addr = self.data.state.hex_window_address.?;
            log.debugf("calculating hex window contents for address 0x{x}", .{addr});

            // @TODO (jrc): hard-coding the length to 256 is just a placeholder. We should
            // detect how many bytes to read based on the size of the viewer window, and
            // accept the number of bytes as a request param.
            const max = @min(math.maxInt(usize) - addr.int(), 256); // avoid integer overflows

            const alloc = self.data.subordinate.?.paused_arena.allocator();
            const buf = try alloc.alloc(u8, max);
            errdefer self.perm_alloc.free(buf);

            if (buf.len > 0) try self.adapter.peekData(
                self.data.subordinate.?.paused.?.pid,
                self.data.subordinate.?.load_addr,
                addr,
                buf,
            );

            const disp = try alloc.alloc(types.HexDisplay, 1);
            disp[0] = .{
                .address = addr,
                .contents = buf,
            };
            self.data.subordinate.?.paused.?.hex_displays = disp;
        }

        fn setWatchExpressions(
            self: *Self,
            scratch: Allocator,
            req: proto.SetWatchExpressionsRequest,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            // free the items in the request (they're allocated in the request queue's allocator)
            defer req.deinit(self.requests.alloc);

            defer self.stateUpdated();

            self.data.mu.lock();
            defer self.data.mu.unlock();

            // clear existing
            self.data.state.clearAndFreeWatchExpressions(self.perm_alloc);

            // set new
            for (req.expressions) |e| try self.data.state.addWatchExpression(self.perm_alloc, e);

            // re-calculate watch expressions if the subordinate is paused
            if (self.data.subordinate != null and self.data.subordinate.?.paused != null) {
                try self.calculateWatchExpressions(scratch);
            }
        }

        fn calculateWatchExpressions(self: *Self, scratch: Allocator) !void {
            const z = trace.zone(@src());
            defer z.end();

            var res = try ArrayList(types.ExpressionResult).initCapacity(
                scratch,
                self.data.state.watch_expressions.items.len,
            );

            for (self.data.state.watch_expressions.items) |expr| {
                const watch = self.calculateExpression(scratch, expr) catch continue;
                res.appendAssumeCapacity(watch);
            }

            self.data.subordinate.?.paused.?.watches = try res.toOwnedSlice();
        }

        fn calculateLocalVariables(self: *Self, scratch: Allocator, locals: []String) !void {
            const z = trace.zone(@src());
            defer z.end();

            var res = try ArrayList(types.ExpressionResult).initCapacity(scratch, locals.len);

            for (locals) |expr| {
                const local = self.calculateExpression(scratch, expr) catch continue;
                res.appendAssumeCapacity(local);
            }

            self.data.subordinate.?.paused.?.locals = try res.toOwnedSlice();
        }

        fn calculateExpression(self: *Self, scratch: Allocator, expression: String) !types.ExpressionResult {
            const z = trace.zone(@src());
            defer z.end();

            var fields = ArrayListUnmanaged(types.ExpressionRenderField){};

            log.debugf("calculating expression: {s}", .{expression});

            const pid = self.data.subordinate.?.paused.?.pid;
            const regs = try self.adapter.getRegisters(pid);
            const pc = regs.pc();
            const load_addr = self.data.subordinate.?.load_addr;

            const func = f: {
                if (self.functionAtAddr(pc)) |func| break :f func;

                log.warnf(
                    "unable to calculate value for expresion \"{s}\": subordinate is stopped in an unknown function",
                    .{expression},
                );
                return error.ExpressionError;
            };
            const cu = self.data.target.?.compile_units[func.cu_ndx.int()];

            const encoder = switch (cu.language) {
                .C => @import("encoding/C.zig").encoder(),
                .Zig => @import("encoding/Zig.zig").encoder(),
                else => return error.LanguageUnsupported,
            };

            const func_frame_base = switch (builtin.target.os.tag) {
                .linux => self.data.target.?.strings.get(func.func.platform_data.frame_base) orelse "",
                else => @compileError("build target not supported"),
            };

            for (func.func.variables) |var_ndx| {
                const variable = cu.variables[var_ndx.int()];
                const var_name = if (self.data.target.?.strings.get(variable.name)) |n| n else continue;
                if (!strings.eql(var_name, expression)) continue;

                var pointers = AutoHashMapUnmanaged(types.Address, types.ExpressionFieldNdx){};
                try self.renderVariableValue(&fields, &pointers, .{
                    .scratch = scratch,
                    .encoder = encoder,
                    .cu = &cu,
                    .pid = pid,
                    .registers = &regs,
                    .load_addr = load_addr,
                    .frame_base = self.data.subordinate.?.paused.?.frame_base_addr,
                    .func_frame_base = func_frame_base,
                    .variable = cu.variables[var_ndx.int()],
                    .expression = expression,
                });
            }

            if (fields.items.len == 0) {
                // we were not able to find the variable, so display "unknown"
                try fields.append(scratch, .{
                    .data = null,
                    .data_type_name = try self.data.subordinate.?.paused.?.strings.add(types.Unknown),
                    .name = try self.data.subordinate.?.paused.?.strings.add(types.Unknown),
                    .encoding = .{ .primitive = .{ .encoding = .string } },
                });
            }

            return types.ExpressionResult{
                .expression = try self.data.subordinate.?.paused.?.strings.add(expression),
                .fields = try fields.toOwnedSlice(scratch),
            };
        }

        const RenderVariableParams = struct {
            scratch: Allocator,
            encoder: encoding.Encoding,
            cu: *const types.CompileUnit,
            pid: types.PID,
            registers: *const arch.Registers,
            load_addr: types.Address,
            frame_base: types.Address,
            func_frame_base: String,
            variable: types.Variable,
            expression: String,

            /// This may be populated if we already know the bytes that represent this
            /// variable (i.e. we're rendering an item in a slice). If it's not populated,
            /// it follows the OS-specific mechanism to look up variable values.
            variable_value_buf: ?[]const u8 = null,
        };

        fn renderVariableValue(
            self: *Self,
            fields: *ArrayListUnmanaged(types.ExpressionRenderField),
            pointers: *AutoHashMapUnmanaged(types.Address, types.ExpressionFieldNdx),
            params: RenderVariableParams,
        ) !void {
            const z = trace.zone(@src());
            defer z.end();

            const var_name = if (self.data.target.?.strings.get(params.variable.name)) |n| n else return;
            if (var_name.len == 0) return;

            const var_platform_data = switch (builtin.target.os.tag) {
                .linux => if (params.variable.platform_data.location_expression) |loc|
                    self.data.target.?.strings.get(loc) orelse ""
                else
                    "",

                else => @compileError("build target not supported"),
            };

            const data_type = params.cu.data_types[params.variable.data_type.int()];
            const data_type_name = self.data.target.?.strings.get(data_type.name) orelse types.Unknown;

            // follow pointer values to their underlying base type
            var base_data_type = data_type;
            var base_data_type_ndx: ?types.TypeNdx = null;
            while (base_data_type.form == .pointer) {
                if (base_data_type.form.pointer.data_type) |ptr_type| {
                    base_data_type = params.cu.data_types[ptr_type.int()];
                    base_data_type_ndx = ptr_type;
                    continue;
                }
                break;
            }
            const base_data_type_name = self.data.target.?.strings.get(base_data_type.name) orelse types.Unknown;

            const buf = blk: {
                // use a pre-determined buffer
                if (params.variable_value_buf) |b| break :blk b;

                // look up the buffer to use in the subordinate registers+memory
                break :blk try self.adapter.getVariableValue(.{
                    .scratch = params.scratch,
                    .pid = params.pid,
                    .registers = params.registers,
                    .load_addr = params.load_addr,
                    .variable_size = data_type.size_bytes,
                    .frame_base = self.data.subordinate.?.paused.?.frame_base_addr,
                    .frame_base_platform_data = params.func_frame_base,
                    .platform_data = var_platform_data,
                });
            };
            const buf_hash = try self.data.subordinate.?.paused.?.strings.add(buf);

            const enc_params = &encoding.Params{
                .scratch = params.scratch,
                .adapter = self.adapter,
                .pid = params.pid,
                .load_addr = params.load_addr,
                .cu = params.cu,
                .target_strings = self.data.target.?.strings,
                .data_type = &data_type,
                .data_type_name = data_type_name,
                .base_data_type = &base_data_type,
                .base_data_type_name = base_data_type_name,
                .val = buf,
            };

            // special-case: strings
            if (params.encoder.isString(enc_params)) |len| {
                const res = try params.encoder.renderString(enc_params, len);
                try fields.append(params.scratch, .{
                    .data = try self.data.subordinate.?.paused.?.strings.add(res.str),
                    .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                    .address = res.address,
                    .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                    .encoding = .{ .primitive = .{ .encoding = .string } },
                });

                return;
            }

            // special-case: slices (known length plus an array)
            if (params.encoder.isSlice(enc_params)) {
                const res = try params.encoder.renderSlice(enc_params);

                try fields.append(params.scratch, .{
                    .data = null,
                    .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                    .address = res.address,
                    .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                    .encoding = .{ .array = .{ .items = undefined } },
                });
                const slice_field_ndx = fields.items.len - 1;

                var item_ndxes = ArrayListUnmanaged(types.ExpressionFieldNdx){};

                // only attempt to render the slice preview if the pointer type isn't opaque
                if (res.item_data_type) |item_data_type| {
                    for (res.item_bufs) |item_buf| {
                        // recursively render slice items using the known item buffer
                        var recursive_params = params;
                        recursive_params.variable_value_buf = item_buf;
                        recursive_params.variable.data_type = item_data_type;

                        const elem_ndx = fields.items.len;
                        try self.renderVariableValue(fields, pointers, recursive_params);
                        if (elem_ndx < fields.items.len) {
                            try item_ndxes.append(params.scratch, types.ExpressionFieldNdx.from(elem_ndx));
                        }
                    }
                }

                // re-assign array items
                fields.items[slice_field_ndx].encoding.array.items = try item_ndxes.toOwnedSlice(params.scratch);
                return;
            }

            switch (data_type.form) {
                .unknown => {
                    log.warnf("unable to calculate value for expresion \"{s}\": variable not found in current function", .{
                        params.expression,
                    });
                    return error.ExpressionError;
                },

                .primitive => |primitive| {
                    try fields.append(params.scratch, .{
                        .data = buf_hash,
                        .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                        .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                        .encoding = .{ .primitive = .{ .encoding = primitive.encoding } },
                    });
                },

                .typedef => |typedef| {
                    if (typedef.data_type == null) {
                        log.warnf("unable to calculate value for expresion \"{s}\": typedef type is opaque", .{
                            params.expression,
                        });
                        return;
                    }

                    // recurse using the base data type
                    var recursive_params = params;
                    recursive_params.variable.data_type = typedef.data_type.?;

                    try self.renderVariableValue(fields, pointers, recursive_params);
                },

                .pointer => {
                    const address = switch (buf.len) {
                        4 => types.Address.from(mem.readVarInt(u32, buf, .little)),
                        8 => types.Address.from(mem.readVarInt(u64, buf, .little)),
                        else => {
                            log.errf("invalid pointer buffer size: {d}", .{buf.len});
                            return error.InvalidPointerBufferLength;
                        },
                    };

                    if (address.eqlInt(0)) {
                        // not sure what to do, just bail out
                        try fields.append(params.scratch, .{
                            .address = address,
                            .data = null,
                            .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                            .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                            .encoding = .{ .primitive = .{ .encoding = .string } },
                        });
                        return;
                    }

                    // check if we've seen this pointer before for this variable
                    if (pointers.get(address)) |field_ndx| {
                        const original_pointer_field = fields.items[field_ndx.int()];
                        try fields.append(params.scratch, original_pointer_field);
                        return;
                    }

                    // the pointer is opaque, so we can't do anything other than render
                    // its address, which may still be useful to the user
                    if (base_data_type_ndx == null or params.encoder.isOpaquePointer(enc_params)) {
                        try fields.append(params.scratch, .{
                            .address = address,
                            .data = null,
                            .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                            .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                            .encoding = .{ .primitive = .{ .encoding = .string } },
                        });
                        return;
                    }

                    // follow typedefs to their base
                    const ptr_type = typedefBaseType(params, base_data_type_ndx.?);

                    // look up the bytes for this pointer's value in the subordinate
                    const ptr_buf = try params.scratch.alloc(u8, ptr_type.data_type.size_bytes);
                    try self.adapter.peekData(params.pid, params.load_addr, address, ptr_buf);

                    // recurse using the base data type
                    var recursive_params = params;
                    recursive_params.variable_value_buf = ptr_buf;
                    recursive_params.variable.data_type = ptr_type.data_type_ndx;

                    const original_len = fields.items.len;

                    // store the pointer value in case we see it again later, thus avoiding
                    // potential stack overflows with circular pointer chains
                    try pointers.put(params.scratch, address, types.ExpressionFieldNdx.from(original_len));

                    try self.renderVariableValue(fields, pointers, recursive_params);

                    // set the pointer value on the new field
                    assert(fields.items.len > original_len);
                    fields.items[original_len].address = address;
                    fields.items[original_len].data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name);
                },

                .array => |arr| {
                    const element_data_type = params.cu.data_types[arr.element_type.int()];

                    // find the array buffer and length of the array (if not already known)
                    var arr_buf: []const u8 = &.{};
                    var arr_len: usize = 0;
                    if (arr.len) |len| {
                        arr_len = len;
                        arr_buf = buf;
                    } else {
                        // @TOOD (jrc): search until a null terminator or N entries
                        // @QUESTION (jrc): when is array length not known? I can't remember at this point.
                        log.warn("unable to render array because length is not known");
                        try fields.append(params.scratch, .{
                            .data = try self.data.subordinate.?.paused.?.strings.add(types.Unknown),
                            .data_type_name = try self.data.subordinate.?.paused.?.strings.add(types.Unknown),
                            .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                            .encoding = .{ .primitive = .{ .encoding = .string } },
                        });
                        return;
                    }

                    // @TODO (jrc): It would be great to also supply the `address` param on this field. We'll
                    // need to pass it back from the adapter, and we'll only be able to know it in some cases,
                    // but it's common enough that it would be nice to have.
                    try fields.append(params.scratch, .{
                        .data = null,
                        .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                        .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                        .encoding = .{ .array = .{ .items = undefined } },
                    });
                    const arr_field_ndx = fields.items.len - 1;

                    var item_ndxes = try ArrayListUnmanaged(types.ExpressionFieldNdx).initCapacity(params.scratch, arr_len);
                    for (0..arr_len) |ndx| {
                        // recursively render array elements using the known item buffer
                        const buf_start = ndx * element_data_type.size_bytes;
                        const buf_end = buf_start + element_data_type.size_bytes;

                        var recursive_params = params;
                        recursive_params.variable_value_buf = arr_buf[buf_start..buf_end];
                        recursive_params.variable.data_type = arr.element_type;

                        try self.renderVariableValue(fields, pointers, recursive_params);
                        const member_ndx = fields.items.len - 1;
                        try item_ndxes.append(params.scratch, types.ExpressionFieldNdx.from(member_ndx));
                    }

                    fields.items[arr_field_ndx].encoding.array.items = try item_ndxes.toOwnedSlice(params.scratch);
                },

                .@"struct" => |strct| {
                    try fields.append(params.scratch, .{
                        .data = null,
                        .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                        .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                        .encoding = .{ .@"struct" = .{ .members = undefined } },
                    });
                    const struct_field_ndx = fields.items.len - 1;

                    var item_ndxes = ArrayListUnmanaged(types.ExpressionFieldNdx){};
                    for (strct.members) |member| {
                        const member_data_type = typedefBaseType(params, member.data_type);
                        const buf_start = member.offset_bytes;
                        const buf_end = buf_start + member_data_type.data_type.size_bytes;

                        // recursively render struct members using the known item buffer
                        var recursive_params = params;
                        recursive_params.variable_value_buf = buf[buf_start..buf_end];
                        recursive_params.variable.data_type = member_data_type.data_type_ndx;

                        const member_ndx = fields.items.len;
                        try self.renderVariableValue(fields, pointers, recursive_params);
                        if (member_ndx < fields.items.len) {
                            // cache and assign the struct member's variable name
                            const member_name = self.data.target.?.strings.get(member.name) orelse types.Unknown;
                            const name_hash = try self.data.subordinate.?.paused.?.strings.add(member_name);
                            fields.items[member_ndx].name = name_hash;

                            try item_ndxes.append(params.scratch, types.ExpressionFieldNdx.from(member_ndx));
                        }
                    }

                    // re-assign slice members
                    fields.items[struct_field_ndx].encoding.@"struct".members = try item_ndxes.toOwnedSlice(params.scratch);
                    return;
                },

                .@"enum" => |enm| {
                    // @TODO (jrc): support non-numeric enum types
                    const enum_val = types.EnumInstanceValue.from(switch (buf.len) {
                        1 => mem.readVarInt(i8, buf, .little),
                        2 => mem.readVarInt(i16, buf, .little),
                        4 => mem.readVarInt(i32, buf, .little),
                        8 => mem.readVarInt(i64, buf, .little),
                        16 => mem.readVarInt(i128, buf, .little),

                        else => {
                            log.errf("invalid enum buffer length: {d}", .{buf.len});
                            return;
                        },
                    });

                    //
                    // The zero'th field describes the type of the enum, and the enum value's name
                    //
                    //

                    var enum_name_hash: ?strings.Hash = null;
                    for (enm.values) |val| {
                        if (val.value == enum_val) {
                            enum_name_hash = try self.data.subordinate.?.paused.?.strings.add(self.data.target.?.strings.get(val.name).?);
                            break;
                        }
                    }

                    try fields.append(params.scratch, .{
                        .data = null,
                        .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                        .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                        .encoding = .{ .@"enum" = .{
                            .value = types.ExpressionFieldNdx.from(fields.items.len),
                            .name = enum_name_hash,
                        } },
                    });

                    //
                    // The rest of the fields are the runtime value of the enum (this can be any type in the
                    // case of i.e. zig's tagged unions)
                    //

                    try fields.append(params.scratch, .{
                        .data = buf_hash,
                        .data_type_name = try self.data.subordinate.?.paused.?.strings.add(data_type_name),
                        .name = enum_name_hash,
                        .encoding = .{ .primitive = .{ .encoding = .signed } },
                    });
                },

                // @DELETEME (jrc): remove the whole else clause
                else => {
                    // append an empty field so the UI renders "unknown"
                    try fields.append(params.scratch, .{
                        .data = null,
                        .data_type_name = try self.data.subordinate.?.paused.?.strings.add(types.Unknown),
                        .name = try self.data.subordinate.?.paused.?.strings.add(var_name),
                        .encoding = .{ .primitive = .{ .encoding = .string } },
                    });

                    log.warnf("unsupported data type: {s}", .{@tagName(data_type.form)});
                },
            }
        }

        /// Follows a given data type that may be a typedef to the first non-typedef type in the chain
        fn typedefBaseType(params: RenderVariableParams, type_ndx: types.TypeNdx) struct {
            data_type: types.DataType,
            data_type_ndx: types.TypeNdx,
        } {
            var data_type = params.cu.data_types[type_ndx.int()];
            var data_type_ndx = type_ndx;
            while (data_type.form == .typedef) {
                if (data_type.form.typedef.data_type) |typedef_type| {
                    data_type = params.cu.data_types[typedef_type.int()];
                    data_type_ndx = typedef_type;
                    continue;
                }
                break;
            }

            return .{
                .data_type = data_type,
                .data_type_ndx = data_type_ndx,
            };
        }
    };
}
