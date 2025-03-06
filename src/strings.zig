const std = @import("std");
const Allocator = mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const AutoHashMapUnmanaged = std.AutoHashMapUnmanaged;
const mem = std.mem;
const Mutex = std.Thread.Mutex;

const flags = @import("flags.zig");
const logging = @import("logging.zig");
const safe = @import("safe.zig");
const trace = @import("trace.zig");

const log = logging.Logger.init(logging.Region.Misc);

/// A string-type alias
pub const String = []const u8;

pub fn eql(a: String, b: String) bool {
    return mem.eql(u8, a, b);
}

pub fn clone(alloc: Allocator, src: String) Allocator.Error!String {
    return try safe.copySlice(u8, alloc, src);
}

/// Hash is a unique, non-cryptographic hash of a string
pub const Hash = u64;

/// Returns a unique, non-cryptographic hash of string for use as an
/// index in to various data structures
pub fn hash(str: String) Hash {
    const z = trace.zone(@src());
    defer z.end();

    return std.hash.Fnv1a_64.hash(str);
}

/// A simple data structure used to do string interning
pub const Cache = struct {
    const Self = @This();

    alloc: Allocator,
    arena: *ArenaAllocator,

    mu: Mutex = .{},
    map: AutoHashMapUnmanaged(Hash, String) = .{},

    pub fn init(alloc: Allocator) Allocator.Error!*Self {
        const z = trace.zone(@src());
        defer z.end();

        const arena = try alloc.create(ArenaAllocator);
        arena.* = ArenaAllocator.init(alloc);
        errdefer {
            arena.deinit();
            alloc.destroy(arena);
        }

        const self = try alloc.create(Self);
        errdefer alloc.destroy(self);
        self.* = .{
            .arena = arena,
            .alloc = arena.allocator(),
        };
        return self;
    }

    pub fn deinit(self: *Self, alloc: Allocator) void {
        const z = trace.zone(@src());
        defer z.end();

        {
            self.mu.lock();
            defer self.mu.unlock();

            self.arena.deinit();
        }

        alloc.destroy(self.arena);
        alloc.destroy(self);
    }

    /// Allocates and does a full copy of every string in the given Cache. Caller
    /// owns returned memory, and the existing cache is still valid after copy.
    pub fn copy(self: *Self, alloc: Allocator) Allocator.Error!*Self {
        const z = trace.zone(@src());
        defer z.end();

        const new = try init(alloc);
        errdefer new.deinit(alloc);

        self.mu.lock();
        defer self.mu.unlock();

        var it = self.map.iterator();
        while (it.next()) |str| {
            _ = try new.add(str.value_ptr.*);
        }

        return new;
    }

    /// Inserts an item in to the cache, returning its cache key. It allocates
    /// and stores a local copy of the string, so the caller is free to do what
    /// it wants with the passed `str`.
    pub fn add(self: *Self, str: String) Allocator.Error!Hash {
        const z = trace.zone(@src());
        defer z.end();

        self.mu.lock();
        defer self.mu.unlock();

        // check for existing so we avoid duplicates
        const hsh = hash(str);
        if (self.map.contains(hsh)) return hsh;

        // copy and store
        const local = try clone(self.alloc, str);
        try self.map.put(self.alloc, hsh, local);
        return hsh;
    }

    /// Do not store or modify the return value of this function. This is intended for
    /// quick-and-dirty lookups of Strings in the table. If the caller wishes to hold
    /// on to the String, use `getOwned`, which will take a copy of the String in the
    /// passed allocator (if any String is found).
    pub fn get(self: *Self, hsh: Hash) ?String {
        const z = trace.zone(@src());
        defer z.end();

        self.mu.lock();
        defer self.mu.unlock();

        return self.map.get(hsh);
    }

    /// Caller owns returned memory, if the String is found
    pub fn getOwned(self: *Self, alloc: Allocator, hsh: Hash) Allocator.Error!?String {
        const z = trace.zone(@src());
        defer z.end();

        self.mu.lock();
        defer self.mu.unlock();

        if (self.map.get(hsh)) |str| {
            return try clone(alloc, str);
        }

        return null;
    }
};
