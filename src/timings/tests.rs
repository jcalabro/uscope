use super::*;

#[test]
fn a_recording_collects_every_threads_spans_and_counts() {
    let recording = Recording::start(Options::default()).expect("no other recording");
    assert_eq!(
        Recording::start(Options::default()).err(),
        Some(AlreadyRecording),
        "one recording at a time"
    );
    {
        let load = crate::span!("load");
        let parent = load.id();
        assert!(parent.is_some(), "a recorded span has an identifier");
        crate::count!("units", 2_usize);
        std::thread::scope(|scope| {
            for unit in 0..2 {
                scope.spawn(move || {
                    let _unit = crate::span!(parent = parent, "unit", "{unit:#x}");
                    {
                        let _inner = crate::span!("decode");
                        crate::count!("dies", 10_u32);
                    }
                    crate::count!("dies", 1_u32);
                });
            }
        });
    }
    crate::count!("loose", 3_u64);
    mark("ready");
    let report = recording.finish();

    let phase = |name: &str| {
        report
            .phases
            .iter()
            .find(|phase| phase.name == name)
            .unwrap_or_else(|| panic!("no phase {name} in {:?}", report.phases))
    };
    assert_eq!(phase("load").spans, 1);
    assert_eq!(phase("load").counters.get("units"), Some(&2));
    assert_eq!(phase("unit").spans, 2);
    assert_eq!(
        phase("unit").counters.get("dies"),
        Some(&2),
        "counts attach to the innermost span"
    );
    assert_eq!(phase("decode").counters.get("dies"), Some(&20));
    assert_eq!(report.counters.get("dies"), Some(&22));
    assert_eq!(report.counters.get("loose"), Some(&3));
    assert!(report.summary.ready_ms.is_some());
    assert_eq!(report.threads.len(), 3, "the caller and two workers");

    let units = report
        .trace_events
        .iter()
        .filter(|event| event.name == "unit")
        .collect::<Vec<_>>();
    let load_id = report
        .trace_events
        .iter()
        .find(|event| event.name == "load")
        .and_then(|event| event.args.get("id").cloned())
        .expect("the load span's identifier");
    assert!(
        units
            .iter()
            .all(|unit| unit.args.get("parent") == Some(&load_id)),
        "work on other threads names the span it ran for"
    );
    let mut labels = units
        .iter()
        .filter_map(|unit| unit.args.get("label")?.as_str())
        .collect::<Vec<_>>();
    labels.sort_unstable();
    assert_eq!(labels, ["0x0", "0x1"]);

    let decoded = Report::from_json(&report.to_json()).expect("the report reads back");
    assert_eq!(decoded.phases.len(), report.phases.len());
    assert!(decoded.text(false).contains("decode"));
    assert!(decoded.threads_text().contains("load: 2 parts"));
}

#[test]
fn nothing_is_measured_without_a_recording() {
    let span = span(
        "idle",
        None,
        Some(&|| panic!("a label formatted while not recording")),
    );
    assert_eq!(span.id(), None);
    count("idle", 1);
    drop(span);

    // Spans begun before a recording are not reported by it.
    let before = crate::span!("before");
    let recording = Recording::start(Options::default()).expect("no other recording");
    drop(before);
    let report = recording.finish();
    assert!(report.phases.is_empty(), "{:?}", report.phases);
}

#[test]
fn a_report_bounds_its_labels() {
    let recording = Recording::start(Options::default()).expect("no other recording");
    drop(crate::span!("long", "{}", "é".repeat(MAX_LABEL)));
    let report = recording.finish();
    let label = report
        .trace_events
        .iter()
        .find_map(|event| event.args.get("label")?.as_str().map(str::to_owned))
        .expect("a label");
    assert!(label.len() <= MAX_LABEL, "{}", label.len());
}

#[test]
fn a_report_says_whether_it_counted_instructions() {
    let recording = Recording::start(Options { instructions: true }).expect("no recording");
    {
        let _work = crate::span!("work");
        std::hint::black_box((0..10_000_u64).sum::<u64>());
    }
    let report = recording.finish();
    let work = &report.phases[0];
    match report.summary.instructions.as_str() {
        "counted" | "multiplexed" => assert!(work.instructions.is_some_and(|count| count > 0)),
        unavailable => {
            assert!(unavailable.starts_with("unavailable: "), "{unavailable}");
            assert_eq!(
                work.instructions, None,
                "an unavailable counter is not zero"
            );
        }
    }
}

mod allocator {
    #![allow(unsafe_code, reason = "these tests drive an allocator directly")]

    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicPtr, Ordering};

    use crate::timings::alloc::{
        Allocated, Counting, PEAK_GRANULARITY, start_totals, stop_totals, thread_counts, totals,
    };

    /// Where the tests' blocks come from: the system's allocator, because
    /// the test process's own counts what it allocates too; under Miri,
    /// Rust's, which Miri serves itself and so checks that every block is
    /// freed with the layout it was allocated with.
    #[cfg(not(miri))]
    type Blocks = std::alloc::System;
    #[cfg(miri)]
    type Blocks = Rust;

    /// Rust's global allocator.
    #[cfg(miri)]
    struct Rust;

    // SAFETY: every method forwards to the global allocator with the
    // caller's arguments.
    #[cfg(miri)]
    unsafe impl GlobalAlloc for Rust {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { std::alloc::alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc_zeroed`.
            unsafe { std::alloc::alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block the global allocator
            // returned, with its layout.
            unsafe { std::alloc::dealloc(block, layout) }
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller passes a block the global allocator
            // returned, its layout, and a valid new size.
            unsafe { std::alloc::realloc(block, layout, new_size) }
        }
    }

    /// Allocates from [`Blocks`], but refuses to grow any block.
    struct NoGrowth;

    // SAFETY: allocation forwards to `Blocks`; refusing a reallocation by
    // returning null is allowed and leaves the block valid.
    unsafe impl GlobalAlloc for NoGrowth {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { Blocks {}.alloc(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block `Blocks` returned.
            unsafe { Blocks {}.dealloc(block, layout) }
        }

        unsafe fn realloc(&self, _: *mut u8, _: Layout, _: usize) -> *mut u8 {
            std::ptr::null_mut()
        }
    }

    /// This thread's counts, from its slot. Other threads of a test
    /// process, such as the harness's, may allocate while a test measures,
    /// and their counts can reach it late, so tests read their own threads'
    /// counts exactly and the process's totals only as bounds.
    fn own() -> (Allocated, i64) {
        thread_counts().expect("a slot of the thread's own")
    }

    #[test]
    fn totals_follow_blocks_across_threads_and_failed_growth() {
        let allocator = Counting::new(NoGrowth);
        // A block that moves the peak by more than a thread's step, so the
        // harness's own allocations cannot hide it.
        let size = 2 * usize::try_from(PEAK_GRANULARITY).expect("a small size");
        let bytes = u64::try_from(size).expect("a small size");
        let layout = Layout::from_size_align(size, 8).expect("a layout");
        start_totals();
        let before = totals();
        // Another thread allocates while this one measures the process's
        // totals, as the test harness's own threads may.
        let other_layout = Layout::from_size_align(48, 8).expect("a layout");
        let other = std::thread::scope(|scope| {
            scope
                // SAFETY: the layout is non-zero; the block is freed below.
                .spawn(|| AtomicPtr::new(unsafe { allocator.alloc(other_layout) }))
                .join()
                .expect("the other thread")
        });
        let (allocated, live) = own();
        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc(layout) };
        assert!(!block.is_null());
        let (grown, grown_live) = own();
        assert_eq!(grown.since(&allocated), Allocated { blocks: 1, bytes });
        assert_eq!(grown_live - live, 2 * PEAK_GRANULARITY);
        let total = totals().since(&before).allocated;
        assert!(
            total.blocks >= 2 && total.bytes >= bytes + 48,
            "the process's totals miss a block: {total:?}"
        );

        // SAFETY: the block came from `allocator` with `layout`.
        let refused = unsafe { allocator.realloc(block, layout, 2 * size) };
        assert!(refused.is_null());
        assert_eq!(
            own(),
            (grown, grown_live),
            "a failed growth allocates nothing"
        );

        // Another thread frees the block, and the heap shrinks all the
        // same.
        let block = AtomicPtr::new(block);
        let (start, end) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let start = own();
                    // SAFETY: the block came from `allocator` with `layout`
                    // and is freed once.
                    unsafe { allocator.dealloc(block.load(Ordering::Relaxed), layout) };
                    (start, own())
                })
                .join()
                .expect("the freeing thread")
        });
        let after = totals();
        stop_totals();
        assert_eq!(end.0, start.0, "freeing allocates nothing");
        assert_eq!(end.1 - start.1, -2 * PEAK_GRANULARITY);
        assert!(
            after.since(&before).peak_bytes >= PEAK_GRANULARITY,
            "{after:?}"
        );
        // SAFETY: the other thread's block came from `allocator` with
        // `other_layout` and is freed once.
        unsafe { allocator.dealloc(other.load(Ordering::Relaxed), other_layout) };
    }

    #[test]
    fn blocks_keep_their_contents_and_counts_as_they_grow_and_shrink() {
        let allocator = Counting::new(Blocks {});
        let pattern: [u8; 16] = *b"0123456789abcdef";
        let layout = |size| Layout::from_size_align(size, 8).expect("a layout");
        let start = own();
        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc_zeroed(layout(16)) };
        assert!(!block.is_null());
        let zeroed = own();
        // SAFETY: the block holds 16 initialized bytes that nothing else
        // refers to.
        let contents = unsafe { std::slice::from_raw_parts_mut(block, 16) };
        assert_eq!(contents, [0; 16]);
        contents.copy_from_slice(&pattern);

        // SAFETY: the block came from `allocator` with this layout.
        let block = unsafe { allocator.realloc(block, layout(16), 64) };
        assert!(!block.is_null());
        let grown = own();
        // SAFETY: growth kept the block's first 16 bytes, which nothing
        // else refers to.
        let contents = unsafe { std::slice::from_raw_parts(block, 16) };
        assert_eq!(contents, pattern);

        // SAFETY: the block came from `allocator` with this layout.
        let block = unsafe { allocator.realloc(block, layout(64), 8) };
        assert!(!block.is_null());
        let shrunk = own();
        // SAFETY: shrinking kept the block's first 8 bytes.
        let contents = unsafe { std::slice::from_raw_parts(block, 8) };
        assert_eq!(contents, &pattern[..8]);

        // SAFETY: the block came from `allocator` with this layout.
        unsafe { allocator.dealloc(block, layout(8)) };
        let freed = own();

        let counted =
            |(allocated, live): (Allocated, i64)| (allocated.since(&start.0), live - start.1);
        let allocated = |blocks, bytes| Allocated { blocks, bytes };
        assert_eq!(counted(zeroed), (allocated(1, 16), 16));
        assert_eq!(
            counted(grown),
            (allocated(2, 64), 64),
            "a growth counts as a block of what it added"
        );
        assert_eq!(counted(shrunk), (allocated(2, 64), 8));
        assert_eq!(counted(freed), (allocated(2, 64), 0));
    }
}
