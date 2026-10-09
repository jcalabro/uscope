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

    use std::alloc::{GlobalAlloc, Layout, System};

    use crate::profile::alloc::{Counting, start_totals, stop_totals, totals};

    /// Allocates from the system, but refuses to grow any block.
    struct NoGrowth;

    // SAFETY: allocation forwards to `System`; refusing a reallocation by
    // returning null is allowed and leaves the block valid.
    unsafe impl GlobalAlloc for NoGrowth {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block `System` returned.
            unsafe { System.dealloc(block, layout) }
        }

        unsafe fn realloc(&self, _: *mut u8, _: Layout, _: usize) -> *mut u8 {
            std::ptr::null_mut()
        }
    }

    #[test]
    fn totals_follow_blocks_across_threads_and_failed_growth() {
        let allocator = Counting::new(NoGrowth);
        let layout = Layout::from_size_align(4096, 8).expect("a layout");
        start_totals();
        let before = totals();
        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc(layout) };
        assert!(!block.is_null());
        let grown = totals().since(&before);
        assert_eq!(grown.allocated.blocks, 1);
        assert_eq!(grown.allocated.bytes, 4096);

        // SAFETY: the block came from `allocator` with `layout`.
        let refused = unsafe { allocator.realloc(block, layout, 8192) };
        assert!(refused.is_null());
        assert_eq!(
            totals().since(&before).allocated,
            grown.allocated,
            "a failed growth allocates nothing"
        );

        // Another thread frees the block, and the heap shrinks all the
        // same. Barriers keep every other allocation outside the window
        // measured, and waiting on one allocates nothing.
        let address = block as usize;
        let barrier = std::sync::Barrier::new(2);
        let (mid, after) = std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                barrier.wait();
                // SAFETY: the block came from `allocator` with `layout` and
                // is freed once.
                unsafe { allocator.dealloc(address as *mut u8, layout) };
                barrier.wait();
                // The thread frees its own blocks as it ends.
                barrier.wait();
            });
            barrier.wait();
            let mid = totals();
            barrier.wait();
            barrier.wait();
            let after = totals();
            barrier.wait();
            (mid, after)
        });
        stop_totals();
        assert_eq!(after.live_bytes - mid.live_bytes, -4096);
        assert!(after.since(&before).peak_bytes >= 4096, "{after:?}");
    }
}
