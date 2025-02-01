//! Reader allows for the easy interpretation of binary data in a given
//! buffer It reads in native endian and is not internally thread-safe.

const std = @import("std");
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const io = std.io;
const leb = std.leb;
const mem = std.mem;
const t = std.testing;

const Reader = @This();

pub const ReadError = error{ EndOfFile, EndOfStream, Overflow };

const IOReader = io.Reader(*Reader, ReadError, readBuf);

off: usize,
buf: []const u8,
io_reader: IOReader,

pub fn init(self: *Reader, buf: []const u8) void {
    self.* = Reader{ .off = 0, .buf = buf, .io_reader = undefined };
    self.io_reader = self.reader();
}

pub fn create(alloc: Allocator, buf: []const u8) error{OutOfMemory}!*Reader {
    const self = try alloc.create(Reader);
    errdefer alloc.destroy(self);

    init(self, buf);
    return self;
}

pub fn reader(self: *Reader) IOReader {
    return .{ .context = self };
}

pub fn readBuf(self: *Reader, dst: []u8) ReadError!usize {
    if (self.buf.len < (self.off + dst.len)) {
        // advance the pointer to the end
        self.off += dst.len;
        return error.EndOfFile;
    }

    const end = self.off + dst.len;
    @memcpy(dst, self.buf[self.off..end]);
    self.off += dst.len;

    return dst.len;
}

pub fn read(self: *Reader, comptime T: type) ReadError!T {
    var subBuf: [@sizeOf(T)]u8 = undefined;
    _ = try self.readBuf(&subBuf);

    return mem.bytesToValue(T, &subBuf);
}

pub fn readUntil(self: *Reader, val: u8) ReadError![]const u8 {
    const start = self.offset();

    const max = std.math.pow(usize, 2, 20);
    for (0..max) |ndx| {
        // @PERFORMANCE (jrc): read in chunks and use SIMD
        const item = try self.read(u8);
        if (item == val) break;

        assert(ndx < max - 1);
    }
    const end = self.offset() - 1;

    assert(end >= start);
    assert(end < self.buf.len);

    return self.buf[start..end];
}

/// seek adjusts our pointer directly to the given offset (absolute, not relative)
pub fn seek(self: *Reader, off: usize) void {
    self.off = off;
}

/// advanceBy skips N bytes
pub fn advanceBy(self: *Reader, off: usize) void {
    self.off = self.off + off;
}

pub fn reset(self: *Reader) void {
    self.seek(0);
}

pub fn offset(self: *Reader) usize {
    return self.off;
}

pub fn atEOF(self: *Reader) bool {
    return self.off >= self.buf.len;
}

/// readSLEB128 can read at most 10 bytes of data, else it
/// will return error.Overflow
pub fn readSLEB128(self: *Reader) ReadError!i64 {
    return try leb.readILEB128(i64, self.io_reader);
}

/// readULEB128 can read at most 10 bytes of data, else it
/// will return error.Overflow
pub fn readULEB128(self: *Reader) ReadError!u64 {
    return try leb.readULEB128(u64, self.io_reader);
}

test "expect various reads to return the correct value" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const alloc = arena.allocator();

    {
        // if there are insufficient remaining bytes in the buffer, it will return EOF
        var r: Reader = undefined;
        r.init(&[_]u8{});
        const res = r.read(u32);
        try t.expectError(error.EndOfFile, res);
    }

    {
        var r = try Reader.create(alloc, &[_]u8{ 0xab, 0, 0, 0xcd });
        const res = try r.read(u32);
        try t.expect(res == 0xcd0000ab);

        const res2 = r.read(u32);
        try t.expectError(error.EndOfFile, res2);
    }

    {
        // multiple successful reads
        var r = try Reader.create(alloc, &[_]u8{ 0x12, 0, 0, 0x34, 0x56, 0, 0, 0x78 });

        var res = try r.read(u32);
        try t.expect(res == 0x34000012);

        res = try r.read(u32);
        try t.expect(res == 0x78000056);

        // back it up and read a larger number, signed this time
        r.reset();
        const res2 = try r.read(i64);
        try t.expect(res2 == 0x7800005634000012);
    }

    {
        // unsigned LEB128
        const buf = &[_]u8{ 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01 };
        var r = try Reader.create(alloc, buf);

        const n = try r.readULEB128();
        try t.expectEqual(@as(u64, std.math.maxInt(u64)), n);
    }

    {
        // multiple ULEB128 reads in a row
        const buf = &[_]u8{
            0x80, 0x3, // 384
            0x80, 0x2, // 256
            0x80, 0x1, // 128
        };
        var r = try Reader.create(alloc, buf);

        try t.expectEqual(@as(u64, 384), try r.readULEB128());
        try t.expectEqual(@as(u64, 256), try r.readULEB128());
        try t.expectEqual(@as(u64, 128), try r.readULEB128());
        try t.expectError(error.EndOfFile, r.readULEB128());
    }

    {
        // signed LEB128
        const buf = &[_]u8{ 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0 };
        var r = try Reader.create(alloc, buf);

        const n = try r.readSLEB128();
        try t.expectEqual(@as(i64, std.math.maxInt(i64)), n);
    }

    {
        // reads of zero-terminated strings
        const buf = &[_]u8{ 'a', 'b', 0, 'c', 'x', 0 };
        var r = try Reader.create(alloc, buf);

        {
            const str = try r.readUntil(0);
            try t.expectEqualSlices(u8, "ab", str);
        }

        {
            const str = try r.readUntil('x');
            try t.expectEqualSlices(u8, "c", str);
        }

        {
            const str = try r.readUntil(0);
            try t.expectEqualSlices(u8, "", str);
        }
    }
}
