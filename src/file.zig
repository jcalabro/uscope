const std = @import("std");
const Allocator = mem.Allocator;
const assert = std.debug.assert;
const AutoHashMapUnmanaged = std.AutoHashMapUnmanaged;
const fs = std.fs;
const math = std.math;
const mem = std.mem;
const Mutex = std.Thread.Mutex;
const posix = std.posix;
const t = std.testing;

const flags = @import("flags.zig");
const logging = @import("logging.zig");
const safe = @import("safe.zig");
const strings = @import("strings.zig");
const String = strings.String;
const trace = @import("trace.zig");

const log = logging.Logger.init(logging.Region.Misc);

/// Hash is a unique, non-cryptographic hash of an absolute file path
pub const Hash = u64;

/// returns a unique, non-cryptographic hash of the absolute file path for use
/// as an index in to various data structures
pub fn hashAbsPath(abs_path: String) Hash {
    const z = trace.zone(@src());
    defer z.end();

    return std.hash.Fnv1a_32.hash(abs_path);
}

pub const SourceFile = struct {
    /// the absolute path of the file
    abs_path: String,

    /// name is the final part of the path (it is a subslice on
    /// the same memory allocation as abs_path, so it should not
    /// be individually free'd on cleanup)
    name: String,
};

const FileMap = AutoHashMapUnmanaged(Hash, SourceFile);

/// Stores and maps file hashes back in to their full names
pub const Cache = struct {
    const Self = @This();

    alloc: Allocator,
    map: FileMap = .{},
    mu: Mutex = .{},

    pub fn init(alloc: Allocator) Allocator.Error!*Self {
        const self = try alloc.create(Self);
        errdefer alloc.destroy(self);
        self.* = .{ .alloc = alloc };
        return self;
    }

    pub fn deinit(self: *Self) void {
        {
            self.mu.lock();
            defer self.mu.unlock();

            var it = self.map.iterator();
            while (it.next()) |item| self.alloc.free(item.value_ptr.*.abs_path);
        }

        self.map.deinit(self.alloc);
        self.alloc.destroy(self);
    }

    pub fn count(self: *Self) usize {
        self.mu.lock();
        defer self.mu.unlock();

        return self.map.count();
    }

    /// Looks up a cached file in the global cache. Returns null if it does not exist.
    pub fn get(self: *Self, fhash: Hash) ?SourceFile {
        const z = trace.zone(@src());
        defer z.end();

        self.mu.lock();
        defer self.mu.unlock();

        return self.map.get(fhash);
    }

    /// Caller does NOT own returned memory, and the strings within the SourceFile are allocated
    /// once on first use, then never mutated again, so they are safe to read from multiple threads
    pub fn getFromPath(self: *Self, abs_path: String) ?SourceFile {
        const z = trace.zone(@src());
        defer z.end();

        const fhash = hashAbsPath(abs_path);
        return self.get(fhash);
    }

    /// Inserts a path in to the cache, returning the hash of the absolute
    /// path. It allocates in he case that the hash does not already exist.
    /// If the entry already exists, this function never returns an error.
    pub fn add(self: *Self, abs_path: String) error{ OutOfMemory, InvalidPath }!Hash {
        const z = trace.zone(@src());
        defer z.end();

        if (!fs.path.isAbsolute(abs_path)) {
            log.errf("expected an absolute path, got: {s}", .{abs_path});
            return error.InvalidPath;
        }

        const file_hash = hashAbsPath(abs_path);
        if (self.get(file_hash) != null) {
            // entry already exists
            return file_hash;
        }

        self.mu.lock();
        defer self.mu.unlock();

        // copy to the local allocator so this cache owns the memory
        const abs = try strings.clone(self.alloc, abs_path);
        errdefer self.alloc.free(abs);

        try self.map.put(self.alloc, file_hash, .{
            .abs_path = abs,
            .name = std.fs.path.basename(abs),
        });

        return file_hash;
    }
};

test "file hashing and caching" {
    const cache = try Cache.init(t.allocator);
    defer cache.deinit();

    const start_count = cache.count();

    const str = "/home/jcalabro/test/file.txt";

    // @TODO (jrc): just use XxHash3 rather than FNV once the self-backend compiler supports it
    // generated using a 3rd party XxHash3/FNV1A32 implementation
    const hash_val = if (flags.LLVM) 0xc75d51990100f90b else 0xebcfc00e;

    for (0..100) |_| {
        const h = try cache.add(str);
        try t.expectEqual(@as(usize, start_count + 1), cache.count());
        try t.expectEqual(hash_val, h);
    }

    {
        // value generated using a 3rd-party hasher
        const val = cache.get(hash_val);
        try t.expect(val != null);
        try t.expectEqualSlices(u8, str, val.?.abs_path);
        try t.expectEqualSlices(u8, "file.txt", val.?.name);
    }
}

pub const LineDelimiter = "\n";

/// Opens the file at the given path, which can be either relative or absolute
pub fn open(path: String, open_flags: fs.File.OpenFlags) fs.File.OpenError!fs.File {
    var dir = fs.cwd();
    return try dir.openFile(path, open_flags);
}

pub const MMapError = posix.MMapError || fs.File.OpenError ||
    fs.File.GetSeekPosError || fs.File.StatError || error{
    FileEmpty,
};

pub fn mapWholeFile(fp: fs.File) MMapError![]align(mem.page_size) const u8 {
    const file_len = math.cast(usize, try fp.getEndPos()) orelse math.maxInt(usize);

    // cannot map a zero-byte file
    if (file_len == 0) return error.FileEmpty;

    const mapped_mem = try posix.mmap(
        null,
        file_len,
        posix.PROT.READ,
        .{ .TYPE = .PRIVATE },
        fp.handle,
        0,
    );
    errdefer posix.munmap(mapped_mem);

    return mapped_mem;
}

pub const munmap = posix.munmap;

/// caller owns returned memory
pub fn readWholeFile(alloc: Allocator, fp: fs.File) !String {
    const fileLen = math.cast(usize, try fp.getEndPos()) orelse math.maxInt(usize);
    return fp.readToEndAlloc(alloc, fileLen);
}
