//! Kernels for uscope, in Zig.
//!
//! A kernel is a WebAssembly function a view calls to walk a structure
//! that is an algorithm more than a layout, as uscope's docs/views.md
//! describes. It exports `run`, which takes the view's arguments as 64-bit
//! words, and yields items of words, such as addresses, which the view
//! presents. It reads the program's memory only through `load` or `read`:
//!
//!     const uscope = @import("uscope_kernel");
//!
//!     export fn run(arguments: [*]const u64, count: i32) i32 {
//!         if (count != 1) return 1;
//!         var node = arguments[0];
//!         while (node != 0) : (node = uscope.load(u64, node)) {
//!             if (!uscope.emit(&.{node})) return 0;
//!         }
//!         return 0;
//!     }
//!
//! Build it with `zig build-exe kernel.zig -target wasm32-freestanding
//! -O ReleaseSmall -fno-entry -rdynamic --dep uscope_kernel
//! -Mroot=kernel.zig -Muscope_kernel=uscope_kernel.zig`.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

/// Fills `buffer` with `length` bytes of the program's memory at
/// `address`, and returns `length`. A read uscope cannot make ends the run,
/// so a kernel never sees one fail; negative results are reserved.
pub extern "uscope_kernel_v1" fn read(address: u64, buffer: [*]u8, length: i32) i32;

/// Yields an item of `count` words, and returns whether uscope wants
/// another. uscope may also stop running the kernel at any call.
pub extern "uscope_kernel_v1" fn yield(words: [*]const u64, count: i32) i32;

/// The value of type `T` at `address` in the program's memory.
pub fn load(comptime T: type, address: u64) T {
    var value: T = undefined;
    _ = read(address, @ptrCast(&value), @sizeOf(T));
    return value;
}

/// Yields an item, and returns whether uscope wants another.
pub fn emit(words: []const u64) bool {
    return yield(words.ptr, @intCast(words.len)) != 0;
}
