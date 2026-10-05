//! Caps the live heap of a test process.
//!
//! A test that allocates without bound, such as a parser that stops consuming
//! its input while it pushes tokens, would otherwise fill the machine's memory
//! across every concurrent test process before any timeout ends it. Past the
//! cap, an allocation fails, which aborts the process with Rust's
//! allocation-failure message after this module names the cap.

#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The most live heap one test process may hold. The heaviest test today
/// peaks near 270 MB of resident memory.
const CAP_BYTES: usize = 1 << 30;

struct CappedAllocator {
    live: AtomicUsize,
}

impl CappedAllocator {
    /// Reserves `bytes` of the cap, or reports why it cannot.
    fn reserve(&self, bytes: usize) -> bool {
        let reserved = self
            .live
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                live.checked_add(bytes).filter(|&total| total <= CAP_BYTES)
            })
            .is_ok();
        if !reserved {
            // Stderr is unbuffered, so this writes without allocating.
            let _ = std::io::stderr().write_all(
                b"an allocation exceeds the test memory cap (tests/support/memory_cap.rs)\n",
            );
        }
        reserved
    }

    fn release(&self, bytes: usize) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System` with the caller's arguments, so
// `System` upholds the allocator contract; the cap only refuses some requests
// by returning null, which the contract allows, and counts only blocks
// `System` actually returned or released.
unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's layout satisfies `alloc`'s requirements.
        let block = unsafe { System.alloc(layout) };
        if block.is_null() {
            self.release(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's layout satisfies `alloc_zeroed`'s requirements.
        let block = unsafe { System.alloc_zeroed(layout) };
        if block.is_null() {
            self.release(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a block this allocator returned, with its
        // layout, and every such block came from `System`.
        unsafe { System.dealloc(block, layout) };
        self.release(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let growth = new_size.saturating_sub(layout.size());
        if !self.reserve(growth) {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller passes a block `System` returned, its layout,
        // and a valid new size.
        let moved = unsafe { System.realloc(block, layout, new_size) };
        if moved.is_null() {
            self.release(growth);
        } else {
            self.release(layout.size().saturating_sub(new_size));
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: CappedAllocator = CappedAllocator {
    live: AtomicUsize::new(0),
};
