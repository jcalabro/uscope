//! Counts allocations for profiles and tests.
//!
//! A binary that wants its allocations reported installs [`Counting`]
//! around the allocator it uses; each thread then counts its own blocks,
//! and while a recording or a test measures totals, the process counts
//! every thread's blocks and the most its heap grew.

#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// What has been allocated: by one thread, or by every thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Allocated {
    /// Blocks allocated, each growth of a block counting as one.
    pub blocks: u64,
    /// Bytes allocated, a block's growth counting only what it added.
    pub bytes: u64,
}

impl Allocated {
    /// What was allocated after `earlier`.
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            blocks: self.blocks.saturating_sub(earlier.blocks),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// Every thread's allocations while totals are counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub allocated: Allocated,
    /// How much the heap grew, net of blocks freed, since counting began:
    /// negative when more was freed than allocated.
    pub live_bytes: i64,
    /// The most the heap had grown since counting began.
    pub peak_bytes: i64,
}

impl Totals {
    /// What was allocated after `earlier`, with the peak growth over it.
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            allocated: self.allocated.since(&earlier.allocated),
            live_bytes: self.live_bytes.saturating_sub(earlier.live_bytes),
            peak_bytes: self.peak_bytes.saturating_sub(earlier.live_bytes),
        }
    }
}

thread_local! {
    static THREAD: Cell<Allocated> = const { Cell::new(Allocated { blocks: 0, bytes: 0 }) };
}

/// Whether some binary counts its allocations with [`Counting`].
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// How many measurements want totals; counting them costs every thread
/// shared atomics, so they are counted only while wanted.
static TOTALS_WANTED: AtomicU64 = AtomicU64::new(0);
static TOTAL_BLOCKS: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

/// What this thread has allocated since it began, or zero when no
/// [`Counting`] allocator is installed.
#[must_use]
pub fn thread_allocated() -> Allocated {
    THREAD.try_with(Cell::get).unwrap_or_default()
}

/// Whether allocations are counted at all.
#[must_use]
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// Starts counting every thread's allocations, until a matching
/// [`stop_totals`]. Measurements may nest.
pub fn start_totals() {
    if TOTALS_WANTED.fetch_add(1, Ordering::AcqRel) == 0 {
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// Stops one measurement's counting of totals.
pub fn stop_totals() {
    let _ = TOTALS_WANTED.try_update(Ordering::AcqRel, Ordering::Relaxed, |wanted| {
        wanted.checked_sub(1)
    });
}

/// Every thread's allocations so far, while totals are counted.
#[must_use]
pub fn totals() -> Totals {
    Totals {
        allocated: Allocated {
            blocks: TOTAL_BLOCKS.load(Ordering::Relaxed),
            bytes: TOTAL_BYTES.load(Ordering::Relaxed),
        },
        live_bytes: LIVE.load(Ordering::Relaxed),
        peak_bytes: PEAK.load(Ordering::Relaxed),
    }
}

/// Resets the peak to the heap's current growth, so that a measurement
/// sees only its own peak.
pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Counts a new block of `bytes`.
#[inline]
fn grew(bytes: usize, new_block: bool) {
    let bytes = bytes as u64;
    let _ = THREAD.try_with(|allocated| {
        let before = allocated.get();
        allocated.set(Allocated {
            blocks: before.blocks + u64::from(new_block),
            bytes: before.bytes + bytes,
        });
    });
    if TOTALS_WANTED.load(Ordering::Relaxed) != 0 {
        TOTAL_BLOCKS.fetch_add(u64::from(new_block), Ordering::Relaxed);
        TOTAL_BYTES.fetch_add(bytes, Ordering::Relaxed);
        let delta = i64::try_from(bytes).unwrap_or(i64::MAX);
        let live = LIVE
            .fetch_add(delta, Ordering::Relaxed)
            .saturating_add(delta);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

/// Counts `bytes` freed.
#[inline]
fn shrank(bytes: usize) {
    if TOTALS_WANTED.load(Ordering::Relaxed) != 0 {
        LIVE.fetch_sub(i64::try_from(bytes).unwrap_or(i64::MAX), Ordering::Relaxed);
    }
}

/// Wraps an allocator to count what each thread, and while wanted the
/// whole process, allocates. Counting allocates nothing itself.
pub struct Counting<A> {
    inner: A,
}

impl<A> Counting<A> {
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

// SAFETY: every method forwards to `inner` with the caller's arguments and
// returns its result unchanged, so `inner` upholds the allocator contract.
// Counting only reads sizes and touches atomics and a constant-initialized
// thread-local cell, none of which allocates or unwinds.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout satisfies `alloc`'s requirements.
        let block = unsafe { self.inner.alloc(layout) };
        if !block.is_null() {
            INSTALLED.store(true, Ordering::Relaxed);
            grew(layout.size(), true);
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout satisfies `alloc_zeroed`'s requirements.
        let block = unsafe { self.inner.alloc_zeroed(layout) };
        if !block.is_null() {
            INSTALLED.store(true, Ordering::Relaxed);
            grew(layout.size(), true);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a block this allocator, and so `inner`,
        // returned, with the layout it was allocated with.
        unsafe { self.inner.dealloc(block, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller passes a block `inner` returned, its layout, and
        // a valid new size.
        let moved = unsafe { self.inner.realloc(block, layout, new_size) };
        // A failed reallocation leaves the old block as it was.
        if !moved.is_null() {
            match new_size.checked_sub(layout.size()) {
                Some(added) => grew(added, added > 0),
                None => shrank(layout.size() - new_size),
            }
        }
        moved
    }
}
