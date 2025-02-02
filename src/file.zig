const std = @import("std");
const Allocator = mem.Allocator;
const assert = std.debug.assert;
const AutoHashMap = std.AutoHashMap;
const fs = std.fs;
const math = std.math;
const mem = std.mem;
const Mutex = std.Thread.Mutex;
const posix = std.posix;
const testing = std.testing;
const ThreadSafeAllocator = std.heap.ThreadSafeAllocator;

const flags = @import("flags.zig");
const logging = @import("logging.zig");
const safe = @import("safe.zig");
const String = @import("strings.zig").String;
const trace = @import("trace.zig");

const log = logging.Logger.init(logging.Region.Misc);

/// Hash is a unique, non-cryptographic hash of an absolute file path
pub const Hash = u64;

/// returns a unique, non-cryptographic hash of the absolute file path for use
/// as an index in to various data structures
pub fn hashAbsPath(abs_path: String) Hash {
    const z = trace.zone(@src());
    defer z.end();

    // @TODO (jrc): just use XxHash3 rather than FNV once the self-hosted backend supports it
    if (flags.LLVM) return std.hash.XxHash3.hash(0, abs_path);
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

const FileMap = AutoHashMap(Hash, SourceFile);

/// file_cache translates file hashes back in to their full names.
/// It keeps its own arena throughout the lifetime of the program.
var file_cache: FileMap = undefined;
var file_cache_mu: Mutex = Mutex{};
var file_thread_safe_allocator: ThreadSafeAllocator = undefined;
var file_allocator: Allocator = undefined;

pub fn initHashCache(alloc: Allocator) void {
    file_cache_mu.lock();
    defer file_cache_mu.unlock();

    file_thread_safe_allocator = .{ .child_allocator = alloc };
    file_allocator = file_thread_safe_allocator.allocator();
    file_cache = FileMap.init(file_allocator);
}

/// Looks up a cached file in the global cache. Returns null if it does not exist.
pub fn getCachedFile(fhash: Hash) ?SourceFile {
    const z = trace.zone(@src());
    defer z.end();

    file_cache_mu.lock();
    defer file_cache_mu.unlock();

    return file_cache.get(fhash);
}

/// Caller does NOT own returned memory, and the strings within the SourceFile are allocated
/// once on first use, then never mutated again, so they are safe to read from multiple threads
pub fn getCachedFileFromAbsPath(abs_path: String) ?SourceFile {
    const z = trace.zone(@src());
    defer z.end();

    const fhash = hashAbsPath(abs_path);
    return getCachedFile(fhash);
}

/// Inserts a path in to the cache, returning the hash of the absolute
/// path. It allocates in he case that the hash does not already exist.
/// If the entry already exists, this function never returns an error.
pub fn addAbsPathToCache(abs_path: String) error{ OutOfMemory, InvalidPath }!Hash {
    const z = trace.zone(@src());
    defer z.end();

    if (!fs.path.isAbsolute(abs_path)) {
        log.errf("expected an absolute path, got: {s}", .{abs_path});
        return error.InvalidPath;
    }

    const file_hash = hashAbsPath(abs_path);
    if (getCachedFile(file_hash) != null) {
        // entry already exists
        return file_hash;
    }

    // copy to the local arena so this cache owns the memory
    const abs = try safe.copySlice(u8, std.heap.c_allocator, abs_path);
    errdefer file_allocator.free(abs);

    file_cache_mu.lock();
    defer file_cache_mu.unlock();

    try file_cache.put(file_hash, .{
        .abs_path = abs,
        .name = std.fs.path.basename(abs),
    });

    return file_hash;
}

test "file hashing and caching" {
    const start_count = file_cache.count();

    const str = "/home/jcalabro/test/file.txt";

    // @TODO (jrc): just use XxHash3 rather than FNV once the self-backend compiler supports it
    // generated using a 3rd party XxHash3/FNV1A32 implementation
    const hash_val = if (flags.LLVM) 0xc75d51990100f90b else 0xebcfc00e;

    for (0..100) |_| {
        const h = try addAbsPathToCache(str);
        try testing.expectEqual(@as(usize, start_count + 1), file_cache.count());
        try testing.expectEqual(hash_val, h);
    }

    {
        // value generated using a 3rd-party hasher
        const val = getCachedFile(hash_val);
        try testing.expect(val != null);
        try testing.expectEqualSlices(u8, str, val.?.abs_path);
        try testing.expectEqualSlices(u8, "file.txt", val.?.name);
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
