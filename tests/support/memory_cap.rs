//! Caps the live heap of a test process, so that a test allocating without
//! bound aborts before it and its concurrent siblings fill the machine's
//! memory. Past the cap an allocation fails, which aborts the process after
//! this module names the cap. It also counts what each thread allocates, so
//! a test can measure the work of something that runs on its own thread.

#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The most live heap one test process may hold, several times the heaviest
/// test's.
const CAP_BYTES: usize = 1 << 30;

/// What a thread has allocated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Allocated {
    /// Blocks allocated, each growth of a block counting as one.
    pub blocks: u64,
    /// Bytes allocated, a block's growth counting only what it added.
    pub bytes: u64,
}

thread_local! {
    static ALLOCATED: Cell<Allocated> = const {
        Cell::new(Allocated { blocks: 0, bytes: 0 })
    };
}

/// What this thread has allocated since it began.
#[allow(
    dead_code,
    reason = "only some test processes measure their allocations"
)]
pub fn allocated() -> Allocated {
    ALLOCATED.with(Cell::get)
}

/// Counts a block of `bytes` this thread allocated. The count needs no
/// allocation, and a thread past its storage's end counts nothing.
fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| {
        let before = allocated.get();
        allocated.set(Allocated {
            blocks: before.blocks + 1,
            bytes: before.bytes + bytes as u64,
        });
    });
}

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
        } else {
            count(layout.size());
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
        } else {
            count(layout.size());
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
            if growth > 0 {
                count(growth);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: CappedAllocator = CappedAllocator {
    live: AtomicUsize::new(0),
};
