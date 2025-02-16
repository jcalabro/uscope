const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const atomic = std.atomic;
const assert = std.debug.assert;
const fs = std.fs;
const fmt = std.fmt;
const Futex = Thread.Futex;
const linux = std.os.linux;
const mem = std.mem;
const Mutex = Thread.Mutex;
const posix = std.posix;
const pow = std.math.pow;
const rand = std.rand;
const SIG = posix.SIG;
const t = std.testing;
const time = std.time;
const Thread = std.Thread;
const ThreadSafeAllocator = std.heap.ThreadSafeAllocator;

const arch = @import("../arch.zig").arch;
const Child = debugger.Child;
const consts = @import("dwarf/consts.zig");
const dwarf = @import("dwarf.zig");
const debugger = @import("../debugger.zig");
const Debugger = debugger.Debugger;
const elf = @import("elf.zig");
const Expression = @import("Expression.zig");
const file = @import("../file.zig");
const flags = @import("../flags.zig");
const frame = @import("dwarf/frame.zig");
const logging = @import("../logging.zig");
const PID = types.PID;
const proto = debugger.proto;
const Queue = @import("../queue.zig").Queue;
const Reader = @import("../Reader.zig");
const safe = @import("../safe.zig");
const strings = @import("../strings.zig");
const trace = @import("../trace.zig");
const types = @import("../types.zig");
const unwind = @import("unwind.zig");
const WaitGroup = std.Thread.WaitGroup;

const log = logging.Logger.init(logging.Region.Linux);

const Self = @This();

const PtraceSetOptions = enum(usize) {
    mask = 0x7f,

    clone = 0x08,
    exec = 0x10,
    exit = 0x40,
    fork = 0x02,
    sysgood = 0x01,
    vfork = 0x04,
    vforkdone = 0x20,

    fn int(self: @This()) usize {
        return @intFromEnum(self);
    }
};

/// Wait extended result codes for trace options
const PtraceEvent = struct {
    const FORK = 1;
    const VFORK = 2;
    const CLONE = 3;
    const EXEC = 4;
    const VFORK_DONE = 5;
    const EXIT = 6;
    const SECCOMP = 7;

    /// Extended result codes which enabled by means other than options
    const STOP = 128;
};

const TrapEvent = struct {
    const BRKPT = 1;
    const TRACE = 2;
    const BRANCH = 3;
    const HWBKPT = 4;
    const UNK = 5;
};

perm_alloc: Allocator = undefined,
shutdown_wg: WaitGroup = .{},

controller_thread_id: atomic.Value(Thread.Id) = atomic.Value(Thread.Id).init(undefined),

/// queue consumer owns allocated memory if async, queue producer owns allocated
/// memory if sync
wait_queue: Queue(*Wait4Request) = undefined,
wait_mu: Mutex = .{},

temp_pause_done: atomic.Value(u32) = atomic.Value(u32).init(DoneVal),

pub fn init(thread_safe_alloc: *ThreadSafeAllocator, req_q: *Queue(proto.Request)) !*Self {
    const perm_alloc = thread_safe_alloc.allocator();
    const self = try perm_alloc.create(Self);
    errdefer perm_alloc.destroy(self);

    const mult = if (flags.Valgrind) 5 else 1;
    self.* = Self{
        .perm_alloc = perm_alloc,
        .wait_queue = Queue(*Wait4Request).init(thread_safe_alloc, .{
            .timeout_ns = mult * time.ns_per_s,
        }),
    };

    self.shutdown_wg.start();
    const wait4Thread = try Thread.spawn(.{}, wait4Loop, .{ self, req_q });
    safe.setThreadName(wait4Thread, "wait4Loop");
    wait4Thread.detach();

    return self;
}

pub fn deinit(self: *Self) void {
    {
        // stop the wait4 loop with a "poison pill"
        const req = self.perm_alloc.create(Wait4Request) catch unreachable;
        req.* = .{
            .pid = undefined,
            .dest = .local_call_site,
            .shutdown = true,
        };
        defer {
            self.wait_mu.lock();
            defer self.wait_mu.unlock();
            self.perm_alloc.destroy(req);
        }

        self.wait_queue.put(req) catch unreachable;
        Futex.timedWait(&req.done, DoneVal, time.ns_per_ms * 50) catch {};
    }

    self.shutdown_wg.wait();

    self.reset();
    self.wait_queue.deinit();
}

/// Clears the adapter to its default state
pub fn reset(self: *Self) void {
    self.wait_queue.reset();
}

/// Loads the given ELF/DWARF data from disk and maps it to a generic Target. Caller
/// owns returned memory, and the allocator must be an arena capable of freeing
/// everything on error or when finished with the data.
pub fn loadDebugSymbols(
    self: *Self,
    alloc: Allocator,
    file_cache: *file.Cache,
    req: proto.LoadSymbolsRequest,
) !*types.Target {
    if (req.path.len == 0) return error.InvalidBinaryPath;

    var scratch = ArenaAllocator.init(self.perm_alloc);
    defer scratch.deinit();

    return try elf.load(&.{
        .perm = alloc,
        .scratch = scratch.allocator(),
        .file_cache = file_cache,
        .path = req.path,
    });
}

fn assertCorrectThreadIsCallingPtrace(self: *Self) void {
    if (builtin.mode == .Debug) {
        assert(self.controller_thread_id.load(.seq_cst) == Thread.getCurrentId());
    }
}

pub fn spawnSubordinate(self: *Self, subordinate: *Child) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    // @NOTE (jrc): the child must have been started with PTRACE_TRACEME
    // after fork() but before exec(), which is why we use our own Child.zig
    subordinate.ptrace_traceme = true;
    try subordinate.spawn();
}

/// Should only be called after the subordinate has spawned and set us its initial SIGTRAP
pub fn setSubordinateTracingOptions(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    var opts: usize = 0;

    // notify when the child spawns a new thread with `clone`
    opts |= PtraceSetOptions.clone.int();

    // notify when the child spawns a new thread with `clone`
    opts |= PtraceSetOptions.clone.int();

    // notify when the child spawns a process with `fork`
    opts |= PtraceSetOptions.fork.int();

    // notify when the child spawns a process with `vfork`
    opts |= PtraceSetOptions.vfork.int();

    // notify when the child or one of its threads is about to exit
    opts |= PtraceSetOptions.clone.int();

    try posix.ptrace(linux.PTRACE.SETOPTIONS, pid.int(), 0, opts);
}

pub fn pauseSubordinate(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    try posix.kill(pid.int(), SIG.STOP);
}

/// temporarilyPauseSubordinate is like pauseSubordinate, but it doesn't call back
/// to the debugger layer via a SubordinateStopRequest. This is useful for when the
/// debugger needs to pause the subordinate for a moment, then continue it (i.e. if
/// we want to set a brekapoint in a process that is currently executing).
pub fn temporarilyPauseSubordinate(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    try posix.kill(pid.int(), SIG.USR2);
    Futex.wait(&self.temp_pause_done, DoneVal);
}

pub fn singleStep(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    try posix.ptrace(linux.PTRACE.SINGLESTEP, pid.int(), 0, 0);
    try self.waitForSignalAsync(pid);
}

pub fn singleStepAndWait(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    try posix.ptrace(linux.PTRACE.SINGLESTEP, pid.int(), 0, 0);
    self.waitForSignalSync(pid, 10 * time.ns_per_ms) catch |err| {
        switch (err) {
            error.Timeout => log.warn("timeout waiting for ptrace singlestep"),
            else => return err,
        }
    };
}

pub fn continueExecution(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    try posix.ptrace(linux.PTRACE.CONT, pid.int(), 0, 0);
}

pub fn getRegisters(self: *Self, pid: types.PID) !arch.Registers {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    // @TODO (jrc): this controls which register set is queried. 1 is the
    // set of general purpose registers, but there may also be floating
    // point and vector registers that are worth querying.
    const NT_PRSTATUS = 1;

    var regs = arch.Registers{};
    const vec = posix.iovec{
        .base = @ptrCast(&regs),
        .len = @sizeOf(@TypeOf(regs)),
    };

    try posix.ptrace(
        linux.PTRACE.GETREGSET,
        pid.int(),
        NT_PRSTATUS,
        @intFromPtr(&vec),
    );

    return regs;
}

pub fn setRegisters(self: *Self, pid: types.PID, regs: *const arch.Registers) !void {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    const NT_PRSTATUS = 1;
    const vec = posix.iovec{
        .base = @ptrCast(@constCast(regs)),
        .len = @sizeOf(@TypeOf(regs.*)),
    };

    try posix.ptrace(
        linux.PTRACE.SETREGSET,
        pid.int(),
        NT_PRSTATUS,
        @intFromPtr(&vec),
    );
}

pub fn peekData(
    self: *Self,
    pid: types.PID,
    load_addr: types.Address,
    read_at_addr: types.Address,
    data: []u8,
) !void {
    self.assertCorrectThreadIsCallingPtrace();
    return globalPeekData(pid, load_addr, read_at_addr, data);
}

fn globalPeekData(
    pid: types.PID,
    load_addr: types.Address,
    read_at_addr: types.Address,
    data: []u8,
) !void {
    const z = trace.zone(@src());
    defer z.end();

    //
    // @SEARCH: PEEKPOKE
    //
    // PTRACE_PEEKDATA reads one word at a time and must align to the
    // word boundary, so we wrap our calls to ensure alignment.
    //
    // We use an internal buffer to guarantee alignment. It's not
    // documented if this is necessary, but we're paranoid.
    //
    // All the same applies to PTRACE_POKEDATA.
    //
    // @REF: https://cs.opensource.google/go/go/+/refs/tags/go1.21.5:src/syscall/syscall_linux.go;l=832
    //

    const addr_size = @sizeOf(usize); // @TODO (jrc): support 32-bit
    var addr = load_addr.add(read_at_addr);

    var num_read: usize = 0;
    var buf align(@alignOf(usize)) = [_]u8{0} ** addr_size;

    const addr_remainder = addr.int() % addr_size;
    if (addr_remainder != 0) {
        // leading edge
        addr = addr.subInt(addr_remainder);
        assert(addr.modInt(addr_size).eqlInt(0));

        try posix.ptrace(
            linux.PTRACE.PEEKDATA,
            pid.int(),
            addr.int(),
            @intFromPtr(&buf),
        );

        num_read += addr_size - addr_remainder;
        if (addr.addIntSafe(addr_size)) |a| addr = a else return;

        const offset = addr_remainder;
        for (0..num_read) |ndx| {
            if (ndx < data.len) {
                data[ndx] = buf[offset + ndx];
            }
        }
    }

    while (num_read < data.len) {
        // remainder
        assert(addr.int() % addr_size == 0);
        try posix.ptrace(
            linux.PTRACE.PEEKDATA,
            pid.int(),
            addr.int(),
            @intFromPtr(&buf),
        );

        // @PERFORMANCE (jrc): might matter for large payloads? Large payloads
        // are probably very, very rare.
        const offset = num_read;
        for (0..addr_size) |ndx| {
            if (offset + ndx >= data.len) break;
            data[offset + ndx] = buf[ndx];
        }

        num_read += addr_size;
        if (addr.addIntSafe(addr_size)) |a| addr = a else return;
    }
}

pub fn pokeData(
    self: *Self,
    pid: types.PID,
    load_addr: types.Address,
    set_at_addr: types.Address,
    data: []const u8,
) !void {
    const z = trace.zone(@src());
    defer z.end();

    //
    // @SEE: PEEKPOKE
    //

    self.assertCorrectThreadIsCallingPtrace();

    const addr_size = @sizeOf(usize);
    var addr = load_addr.add(set_at_addr);

    var num_written: usize = 0;
    var buf align(@alignOf(usize)) = [_]u8{0} ** addr_size;

    {
        //
        // Leading edge
        //

        const addr_remainder = addr.int() % addr_size;
        if (addr_remainder != 0) {
            if (addr.subIntSafe(addr_remainder)) |a| addr = a else return;
            assert(addr.modInt(addr_size).eqlInt(0));

            try posix.ptrace(
                linux.PTRACE.PEEKDATA,
                pid.int(),
                addr.int(),
                @intFromPtr(&buf),
            );

            // copy the data we want to write to the back
            // part of what already exists in memory
            const num_replacements = addr_size - addr_remainder;
            const offset = addr_remainder;
            for (0..num_replacements) |ndx| {
                if (ndx < data.len) {
                    buf[offset + ndx] = data[ndx];
                }
            }

            // @NOTE (jrc): POKEDATA's data argument takes the word-sized
            // value that should be set, _not_ a pointer to the word
            const word = mem.readInt(usize, &buf, .little);

            try posix.ptrace(
                linux.PTRACE.POKEDATA,
                pid.int(),
                addr.int(),
                word,
            );

            num_written += num_replacements;
            if (addr.addIntSafe(addr_size)) |a| addr = a else return;
        }
    }

    {
        //
        // Interior
        //

        // we're done, exit early to avoid integer underflow
        if (num_written >= data.len) return;

        const len = (data.len - num_written) / addr_size;
        while (num_written < len) {
            const end = num_written + addr_size;
            @memcpy(&buf, data[num_written..end]);

            const word = mem.readInt(usize, &buf, .little);

            assert(addr.modInt(addr_size).eqlInt(0));
            try posix.ptrace(
                linux.PTRACE.POKEDATA,
                pid.int(),
                addr.int(),
                word,
            );

            num_written += addr_size;
            if (addr.addIntSafe(addr_size)) |a| addr = a else return;
        }
    }

    {
        //
        // Trailing edge
        //

        // we're done, exit early to avoid integer underflow
        if (num_written >= data.len) return;

        const remaining = data.len - num_written;
        if (remaining > 0) {
            assert(addr.modInt(addr_size).eqlInt(0));
            try posix.ptrace(
                linux.PTRACE.PEEKDATA,
                pid.int(),
                addr.int(),
                @intFromPtr(&buf),
            );

            // copy the data we want to write to the front
            // part of what already exists in memory
            for (0..remaining) |ndx| {
                buf[ndx] = data[num_written + ndx];
            }

            const word = mem.readInt(usize, &buf, .little);

            try posix.ptrace(
                linux.PTRACE.POKEDATA,
                pid.int(),
                addr.int(),
                word,
            );
        }
    }
}

pub fn setBreakpoint(
    self: *Self,
    load_addr: types.Address,
    bp: *types.Breakpoint,
    pid: types.PID,
) !types.ThreadBreakpoint {
    const z = trace.zone(@src());
    defer z.end();

    var buf = [_]u8{0};
    try self.peekData(pid, load_addr, bp.addr, &buf);
    const low_byte = buf[0];

    buf[0] = arch.InterruptInstruction;
    try self.pokeData(pid, load_addr, bp.addr, &buf);

    bp.instruction_byte = low_byte;
    return .{
        .bid = bp.bid,
        .pid = pid,
    };
}

pub fn unsetBreakpoint(
    self: *Self,
    load_addr: types.Address,
    bp: types.Breakpoint,
    pid: types.PID,
) !void {
    const z = trace.zone(@src());
    defer z.end();

    var buf = [_]u8{bp.instruction_byte};
    try self.pokeData(pid, load_addr, bp.addr, &buf);
}

/// After the subordinate has paused, this call checks whether or not a new
/// thread was created in the subordinate via a `clone` call.
pub fn handleEvent(self: *Self, pid: types.PID) !?debugger.SubordinateEvent {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    var siginfo = mem.zeroes(posix.siginfo_t);
    try posix.ptrace(linux.PTRACE.GETSIGINFO, pid.int(), 0, @intFromPtr(&siginfo));

    if (siginfo.signo == SIG.TRAP) {
        switch (siginfo.code) {
            PtraceEvent.STOP => return null,

            SIG.TRAP | (PtraceEvent.FORK << 8),
            SIG.TRAP | (PtraceEvent.VFORK << 8),
            SIG.TRAP | (PtraceEvent.CLONE << 8),
            => {
                // a new thread was spawned, get its PID
                var cloned_pid: c_long = 0;
                try posix.ptrace(linux.PTRACE.GETEVENTMSG, pid.int(), 0, @intFromPtr(&cloned_pid));
                assert(cloned_pid > 0);
                return .{ .new_thread_spawned = types.PID.from(@intCast(cloned_pid)) };
            },

            SIG.TRAP | (PtraceEvent.EXEC << 8) => {
                log.warn("EVENT: EXEC");
            },
            SIG.TRAP | (PtraceEvent.EXIT << 8) => {
                log.warn("EVENT: EXIT");
            },
            0, TrapEvent.TRACE => {
                log.warn("EVENT: TRACE");
            },
            SIG.TRAP, SIG.TRAP | 0x80 => {
                log.warn("EVENT: TRAP");
            },

            else => {
                log.errf("unknown ptrace SIGTRAP code: {d}", .{siginfo.code});
                return null;
            },
        }
    }

    return null;
}

const Wait4Flags = struct {
    const WNOTHREAD = 0x20000000;
    const WALL = 0x40000000;
    const WCLONE = 0x80000000;
};

/// wait4Loop spawns a loop that runs on a background thread forever, waiting for results
/// to come in from the subordinate process
fn wait4Loop(self: *Self, req_queue: *Queue(proto.Request)) void {
    trace.initThread();
    defer trace.deinitThread();

    const z = trace.zone(@src());
    defer z.end();

    defer self.shutdown_wg.finish();

    while (true) {
        // wait for a signal that tells us to start the wait4 call
        var req = self.wait_queue.get() catch continue;
        defer {
            self.wait_mu.lock();
            defer self.wait_mu.unlock();

            switch (req.dest) {
                .local_call_site => Futex.wake(&req.done, 1),
                .debugger_thread => {
                    self.perm_alloc.destroy(req);
                },
            }
        }

        const req_local = blk: {
            self.wait_mu.lock();
            defer self.wait_mu.unlock();

            // the debugger is fully shutting down, stop the loop
            if (req.shutdown) return;
            break :blk req.*;
        };

        // the debugger is fully shutting down, stop the loop
        if (req_local.shutdown) return;

        const res = waitpid(types.PID.from(-1), Wait4Flags.WALL) catch {
            log.warnf("thread {d} forcibly exited", .{req_local.pid.int()});
            continue;
        };

        const status = WaitStatus{ .status = res.status };

        var should_stop_debugger = true;
        if (status.stopped()) {
            switch (status.stopSignal()) {
                SIG.WINCH => should_stop_debugger = false,
                else => {},
            }
        }

        if (status.exitStatus() == SIG.USR2) {
            // a temporary pause happened, don't call back the debugger layer,
            // just call back the immediate call site
            Futex.wake(&self.temp_pause_done, 1);
            continue;
        }

        if (status.exited()) {
            log.debugf("thread {d} exited with status: {d}", .{
                req_local.pid.int(),
                status.exitStatus(),
            });
        }

        switch (req_local.dest) {
            .local_call_site => {}, // nothing to do
            .debugger_thread => {
                // inform the main debugger thread that the subordinate was stopped
                const stopped_req = proto.SubordinateStoppedRequest{
                    .pid = req_local.pid,
                    .flags = .{
                        .exited = status.exited() or (status.signaled() and status.terminationSignal() == 9),
                        .should_stop_debugger = should_stop_debugger,
                    },
                };

                trace.message(@tagName(stopped_req.req()));
                req_queue.put(stopped_req.req()) catch |err| {
                    log.errf("unable to enqueue command {s}: {!}", .{
                        @typeName(@TypeOf(stopped_req)),
                        err,
                    });
                };
            },
        }
    }
}

/// We use our own fork of wait4 because the zig stdlib doesn't gracefully handle the
/// case where the subordinate process is forcibly terminated (the .CHILD case)
fn waitpid(pid: types.PID, wait_flags: u32) !posix.WaitPidResult {
    var status: if (builtin.link_libc) c_int else u32 = undefined;
    while (true) {
        const rc = posix.system.waitpid(pid.int(), &status, @intCast(wait_flags));
        switch (posix.errno(rc)) {
            .SUCCESS => return .{
                .pid = @intCast(rc),
                .status = @bitCast(status),
            },
            .INTR => continue,
            .CHILD => {
                // process either was never running or has been forcibly terminated
                return error.ProcessDoesNotExist;
            },
            .INVAL => unreachable, // Invalid flags.
            else => unreachable,
        }
    }
}

/// @REF (jrc): https://github.com/lattera/glibc/blob/master/bits/waitstatus.h
///             https://github.com/lattera/glibc/blob/master/posix/sys/wait.h#L54-L63
pub const WaitStatus = struct {
    /// The result of the waitpid/wait4 call
    status: u32,

    /// Returns true if the subordinate process exited normally
    pub fn exited(self: @This()) bool {
        return (self.status & 0x7f) == 0;
    }

    /// Returns the exit code of the subordinate process if `exited()`
    pub fn exitStatus(self: @This()) u8 {
        return @intCast((self.status >> 8) & 0xff);
    }

    /// Returns true if the subordinate process was terminated by a signal (not to be
    /// confused with `stopped()`)
    pub fn signaled(self: @This()) bool {
        return (self.status & 0x7f) != 0 and (self.status & 0x7f) != 0x7f;
    }

    /// Returns the terminating signal of the subordinate process if `signaled()`
    pub fn terminationSignal(self: @This()) u8 {
        return @intCast(self.status & 0x7f);
    }

    /// Returns true if the subordinate process was stopped by a signal (not to be confused
    /// with `signaled()`)
    pub fn stopped(self: @This()) bool {
        return (self.status & 0xff) == 0x7f;
    }

    /// Returns the stopping signal of the subordinate process if `stopped()`
    pub fn stopSignal(self: @This()) u8 {
        return @intCast((self.status >> 8) & 0xff);
    }
};

/// Indicates where we will forward the signal after a wait4 call finishes
pub const Wait4SignalDest = enum(u8) {
    local_call_site,
    debugger_thread,
};

const Wait4Request = struct {
    pid: types.PID,
    dest: Wait4SignalDest,

    /// Contains the "status" field of the WaitStatus result
    done: atomic.Value(u32) = atomic.Value(u32).init(DoneVal),

    shutdown: bool = false,
};

const DoneVal = 1;

pub fn waitForSignalSync(self: *Self, pid: types.PID, timeout_ns: u64) !void {
    const z = trace.zone(@src());
    defer z.end();

    const req = blk: {
        self.wait_mu.lock();
        defer self.wait_mu.unlock();

        const r = try self.perm_alloc.create(Wait4Request);
        r.* = .{
            .pid = pid,
            .dest = .local_call_site,
        };
        break :blk r;
    };
    defer {
        self.wait_mu.lock();
        defer self.wait_mu.unlock();
        self.perm_alloc.destroy(req);
    }

    try self.wait_queue.put(req);
    try Futex.timedWait(&req.done, DoneVal, timeout_ns);
}

pub fn waitForSignalAsync(self: *Self, pid: types.PID) !void {
    const z = trace.zone(@src());
    defer z.end();

    const req = blk: {
        // @NOTE (jrc): we need to take this lock because the wait4 loop also
        // accesses the memory we allocate, and the allocator may re-use the
        // memory free up, which leads to race conditions/data corruption
        self.wait_mu.lock();
        defer self.wait_mu.unlock();

        const r = try self.perm_alloc.create(Wait4Request);
        errdefer self.perm_alloc.destroy(r);

        r.* = .{
            .pid = pid,
            .dest = .debugger_thread,
        };
        break :blk r;
    };

    try self.wait_queue.put(req);
}

/// @QUESTION (jrc): Can we use std.os.linux.getauxval(std.elf.AT_BASE) instead of parsing the file?
/// Something like: https://github.com/ziglang/zig/blob/812557bfde3c577b5f00cb556201c71ad5ed6fa4/lib/std/process.zig#L1616-L1632
pub fn parseLoadAddressFromFile(scratch: Allocator, pid: types.PID) !types.Address {
    const z = trace.zone(@src());
    defer z.end();

    const map_path = try fmt.allocPrint(scratch, "/proc/{d}/maps", .{pid});
    defer scratch.free(map_path);

    const fp = try fs.openFileAbsolute(map_path, .{});
    defer fp.close();

    const contents = try fp.readToEndAlloc(scratch, pow(usize, 2, 16));
    defer scratch.free(contents);

    return try parseLoadAddress(contents);
}

fn parseLoadAddress(contents: []const u8) !types.Address {
    var line_it = mem.splitSequence(u8, contents, "\n");
    while (line_it.next()) |line| {
        var is_base_addr_line = true;
        var token_it = mem.splitBackwardsSequence(u8, line, " ");
        while (token_it.next()) |token| {
            if (token.len == 0) continue;

            if (token[0] == '[') {
                is_base_addr_line = false;
                break;
            }
        }

        if (is_base_addr_line) return try parseLoadAddressFromLine(line);
    }

    return error.InvalidProcMapsFile;
}

fn parseLoadAddressFromLine(line: []const u8) !types.Address {
    const max = 32;
    var buf = [_]u8{0} ** max;

    var ndx: usize = 0;
    while (ndx < max) : (ndx += 1) {
        if (ndx >= line.len) return error.InvalidProcMapsFile;
        if (line[ndx] == '-') break;
        buf[ndx] = line[ndx];
    }

    if (ndx == 0 or ndx >= max) return error.InvalidProcMapsFile;

    const load_addr = try fmt.parseInt(usize, buf[0..ndx], 16);
    return types.Address.from(load_addr);
}

test "linux: parse load address" {
    const maps =
        \\5630231d2000-5630231d8000 r--p 00000000 00:23 9310890                    /home/jcalabro/go/src/github.com/jcalabro/uscope/assets/rustloop/out
        \\5630231d8000-563023218000 r-xp 00006000 00:23 9310890                    /home/jcalabro/go/src/github.com/jcalabro/uscope/assets/rustloop/out
        \\563023218000-563023227000 r--p 00046000 00:23 9310890                    /home/jcalabro/go/src/github.com/jcalabro/uscope/assets/rustloop/out
        \\563023227000-56302322a000 r--p 00054000 00:23 9310890                    /home/jcalabro/go/src/github.com/jcalabro/uscope/assets/rustloop/out
        \\56302322a000-56302322b000 rw-p 00057000 00:23 9310890                    /home/jcalabro/go/src/github.com/jcalabro/uscope/assets/rustloop/out
        \\563024188000-5630241a9000 rw-p 00000000 00:00 0                          [heap]
        \\7f133493b000-7f133493e000 rw-p 00000000 00:00 0
        \\7f133493e000-7f1334964000 r--p 00000000 00:23 3821002                    /usr/lib64/libc.so.6
        \\7f1334964000-7f1334ac1000 r-xp 00026000 00:23 3821002                    /usr/lib64/libc.so.6
        \\7f1334ac1000-7f1334b0e000 r--p 00183000 00:23 3821002                    /usr/lib64/libc.so.6
        \\7f1334b0e000-7f1334b12000 r--p 001d0000 00:23 3821002                    /usr/lib64/libc.so.6
        \\7f1334b12000-7f1334b14000 rw-p 001d4000 00:23 3821002                    /usr/lib64/libc.so.6
        \\7f1334b14000-7f1334b1c000 rw-p 00000000 00:00 0
        \\7f1334b1c000-7f1334b1f000 r--p 00000000 00:23 3828363                    /usr/lib64/libgcc_s-13-20231011.so.1
        \\7f1334b1f000-7f1334b3a000 r-xp 00003000 00:23 3828363                    /usr/lib64/libgcc_s-13-20231011.so.1
        \\7f1334b3a000-7f1334b3e000 r--p 0001e000 00:23 3828363                    /usr/lib64/libgcc_s-13-20231011.so.1
        \\7f1334b3e000-7f1334b3f000 r--p 00021000 00:23 3828363                    /usr/lib64/libgcc_s-13-20231011.so.1
        \\7f1334b3f000-7f1334b40000 rw-p 00000000 00:00 0
        \\7f1334b58000-7f1334b59000 ---p 00000000 00:00 0
        \\7f1334b59000-7f1334b5d000 rw-p 00000000 00:00 0
        \\7f1334b5d000-7f1334b5e000 r--p 00000000 00:23 3820999                    /usr/lib64/ld-linux-x86-64.so.2
        \\7f1334b5e000-7f1334b85000 r-xp 00001000 00:23 3820999                    /usr/lib64/ld-linux-x86-64.so.2
        \\7f1334b85000-7f1334b8f000 r--p 00028000 00:23 3820999                    /usr/lib64/ld-linux-x86-64.so.2
        \\7f1334b8f000-7f1334b91000 r--p 00031000 00:23 3820999                    /usr/lib64/ld-linux-x86-64.so.2
        \\7f1334b91000-7f1334b93000 rw-p 00033000 00:23 3820999                    /usr/lib64/ld-linux-x86-64.so.2
        \\7ffcc72f1000-7ffcc7314000 rw-p 00000000 00:00 0                          [stack]
        \\7ffcc731c000-7ffcc7320000 r--p 00000000 00:00 0                          [vvar]
        \\7ffcc7320000-7ffcc7322000 r-xp 00000000 00:00 0                          [vdso]
        \\ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]
    ;

    try t.expectEqual(types.Address.from(0x5630231d2000), try parseLoadAddress(maps));
}

test "linux: parse load address of PIE" {
    const maps =
        \\7fe51fc4a000-7fe51fc4e000 r--p 00000000 00:00 0                          [vvar]
        \\7fe51fc4e000-7fe51fc50000 r-xp 00000000 00:00 0                          [vdso]
        \\7fe51fc50000-7fe51fc98000 r--p 00000000 103:08 30172053                  /home/jcalabro/go/src/github.com/jcalabro/zchess/zig-out/bin/tests
        \\7fe51fc98000-7fe51fda0000 r-xp 00047000 103:08 30172053                  /home/jcalabro/go/src/github.com/jcalabro/zchess/zig-out/bin/tests
        \\7fe51fda0000-7fe51fda6000 rw-p 0014e000 103:08 30172053                  /home/jcalabro/go/src/github.com/jcalabro/zchess/zig-out/bin/tests
        \\7fe51fda6000-7fe51fda7000 rw-p 00153000 103:08 30172053                  /home/jcalabro/go/src/github.com/jcalabro/zchess/zig-out/bin/tests
        \\7fe51fda7000-7fe51fdaf000 rw-p 00000000 00:00 0
        \\7ffe34d70000-7ffe34d92000 rw-p 00000000 00:00 0                          [stack]
        \\ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]
    ;

    try t.expectEqual(types.Address.from(0x7fe51fc50000), try parseLoadAddress(maps));
}

pub fn unwindStack(
    self: *Self,
    scratch: Allocator,
    pid: types.PID,
    load_addr: types.Address,
    regs: *const arch.Registers,
    addr_size: types.AddressSize,
    cie: *const frame.CIE,
    depth: ?u32, // null indicates that we should unwind the entire stack
) !types.UnwindResult {
    return try unwind.stack(
        self,
        scratch,
        pid,
        regs,
        load_addr,
        addr_size,
        cie,
        depth,
    );
}

pub const GetVariableValueArgs = struct {
    scratch: Allocator,
    pid: types.PID,
    registers: *const arch.Registers,
    load_addr: types.Address,
    variable_size: u64,
    frame_base: types.Address,
    frame_base_platform_data: []const u8,
    platform_data: []const u8,
};

/// Passed allocator must be a scratch arena, and the caller owns returned memory.
pub fn getVariableValue(self: *Self, args: GetVariableValueArgs) ![]const u8 {
    const z = trace.zone(@src());
    defer z.end();

    self.assertCorrectThreadIsCallingPtrace();

    var expr = Expression{
        .alloc = args.scratch,
        .pid = args.pid,
        .registers = args.registers,
        .load_addr = args.load_addr,
        .variable_size = args.variable_size,
        .frame_base = args.frame_base,
        .frame_base_location_expr = args.frame_base_platform_data,
        .location_expression = args.platform_data,
    };

    return try expr.evaluate(globalPeekData);
}
