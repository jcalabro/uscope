use super::builder::LineInput;
use super::*;

/// A small capture with two threads, shared stacks, symbols, and a zone.
fn sample_capture(offset: i64) -> Capture {
    let mut builder = CaptureBuilder::new();
    builder.setting("start", &offset.to_string());
    builder.setting("end", &(offset + 100).to_string());
    let module = builder.module("/bin/app", Some("abcd"), [7; 16], 4096);
    let main_thread = builder.thread(10, 10, offset);
    let worker = builder.thread(10, 11, offset + 1);
    builder.thread_name(worker, "worker");
    let main = builder.frame(FrameKind::Native, module, 0x1000, 0);
    let work = builder.frame(FrameKind::Native, module, 0x2000, 0);
    let cut = builder.truncation(Truncation::FramePointerNotKept, module);
    builder.symbolize(
        main,
        module,
        &[LineInput {
            function: "main",
            file: Some("app.c"),
            line: 3,
            column: 1,
            declared: Some(("app.c", 1)),
            role: 0,
        }],
    );
    let deep = builder.stack(&[work, main]);
    let shallow = builder.stack(&[main]);
    let broken = builder.stack(&[work, cut]);
    builder.sample(main_thread, offset + 5, 1000, deep, NONE);
    builder.sample(main_thread, offset + 2, 1000, shallow, NONE);
    builder.sample(worker, offset + 3, 1000, broken, NONE);
    builder.off_cpu(worker, offset + 4, 2, false);
    builder.off_cpu(worker, offset + 1, 1, true);
    builder.count(AccountKind::LostSamples as u32, 2);
    builder.gap(offset + 6, offset + 7, GapReason::Budget);
    builder.marker(
        offset + 6,
        MarkerKind::Budget,
        "sampling half of each second",
    );
    let site = builder.site("handle", "app.rs", 9, module, 1);
    builder.zone(
        worker,
        Zone {
            start: (offset + 3).into(),
            end: (offset + 9).into(),
            site: site.into(),
            flags: 0.into(),
            value: NONE.into(),
            reserved: 0.into(),
        },
    );
    builder.finish()
}

#[test]
fn a_capture_seals_reads_back_and_files_merge_chunks_by_identity() {
    let capture = sample_capture(0);
    assert_eq!(capture.validate(), Ok(()));
    // Samples come out per thread, in time order.
    let first = capture.thread_samples(&capture.sample_ranges[0]);
    assert_eq!(
        first
            .iter()
            .map(|sample| sample.time.get())
            .collect::<Vec<_>>(),
        [2, 5]
    );
    let chunk = capture.seal().expect("seal");
    assert_eq!(
        Capture::from_chunk(chunk.as_bytes()).expect("read"),
        capture
    );

    // A file of two chunks of the same recording merges them: frames and
    // stacks are interned once, and each thread's samples stay ordered.
    let mut bytes = Vec::new();
    let mut writer = file::Writer::new(&mut bytes).expect("header");
    writer.chunk(&capture.seal().expect("seal")).expect("chunk");
    let later = {
        let mut capture = sample_capture(0);
        for sample in &mut capture.samples {
            sample.time = (sample.time.get() + 50).into();
        }
        for zone in &mut capture.zones {
            zone.start = (zone.start.get() + 50).into();
            zone.end = (zone.end.get() + 50).into();
        }
        for gap in &mut capture.gaps {
            gap.start = (gap.start.get() + 50).into();
            gap.end = (gap.end.get() + 50).into();
        }
        capture
    };
    writer.chunk(&later.seal().expect("seal")).expect("chunk");
    writer.finish().expect("index");
    let read = file::read(&bytes).expect("read the file");
    assert_eq!((read.chunks, read.truncated), (2, false));
    let merged = read.capture;
    assert_eq!(merged.validate(), Ok(()));
    assert_eq!(merged.frames.len(), capture.frames.len());
    assert_eq!(merged.stacks.len(), capture.stacks.len());
    assert_eq!(merged.threads.len(), 2);
    assert_eq!(merged.samples.len(), 6);
    assert_eq!(merged.zones.len(), 2);
    assert_eq!(merged.gaps.len(), 2);
    assert_eq!(merged.account(AccountKind::LostSamples as u32), 4);
    assert_eq!(merged.frame_lines(0).len(), 1);

    // A file cut inside its index, or inside its second chunk, still reads
    // every complete chunk, and says it was cut.
    let cut = &bytes[..bytes.len() - 3];
    let read = file::read(cut).expect("read a cut file");
    assert_eq!((read.chunks, read.truncated), (2, true));
    let first_chunk = 16 + 8 + capture.seal().expect("seal").as_bytes().len();
    let read = file::read(&bytes[..first_chunk + 100]).expect("read a file cut in a chunk");
    assert_eq!((read.chunks, read.truncated), (1, true));
}

/// Forgetting what came before a moment keeps every row from it on, cuts
/// the spans that cross it there, and keeps what rows refer to.
#[test]
fn a_builder_forgets_rows_before_a_moment() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(1, 1, 0);
    let frame = builder.frame(FrameKind::Native, NONE, 0x10, 0);
    let stack = builder.stack(&[frame]);
    for time in [1, 5, 9] {
        builder.sample(thread, time, 1, stack, NONE);
    }
    builder.off_cpu(thread, 2, 2, false);
    builder.off_cpu(thread, 4, 4, true);
    builder.gap(1, 2, GapReason::Budget);
    builder.gap(3, 7, GapReason::Budget);
    builder.marker(3, MarkerKind::Budget, "before");
    builder.marker(6, MarkerKind::Budget, "after");
    builder.forget_before(5);
    let capture = builder.finish();
    assert_eq!(capture.validate(), Ok(()));
    let times = |rows: &[Sample]| rows.iter().map(|row| row.time.get()).collect::<Vec<_>>();
    assert_eq!(times(&capture.samples), [5, 9]);
    assert_eq!(
        capture
            .off_cpu
            .iter()
            .map(|row| (row.time.get(), row.duration.get(), row.preempted))
            .collect::<Vec<_>>(),
        [(5, 3, 1)]
    );
    assert_eq!(
        capture
            .gaps
            .iter()
            .map(|gap| (gap.start.get(), gap.end.get()))
            .collect::<Vec<_>>(),
        [(5, 7)]
    );
    assert_eq!(capture.markers.len(), 1);
    assert_eq!(capture.stacks.len(), 1);
}

#[test]
fn validation_rejects_every_out_of_bounds_reference() {
    let valid = sample_capture(0);
    let mut broken = valid.clone();
    broken.stacks[0].parent = 5.into();
    assert!(broken.validate().is_err(), "a parent after its child");
    let mut broken = valid.clone();
    broken.samples[0].stack = 99.into();
    assert!(broken.validate().is_err(), "a sample of no stack");
    let mut broken = valid.clone();
    broken.samples.swap(0, 1);
    assert!(broken.validate().is_err(), "samples out of order");
    let mut broken = valid.clone();
    broken.off_cpu.swap(0, 1);
    assert!(broken.validate().is_err(), "off-CPU intervals out of order");
    let mut broken = valid.clone();
    broken.off_cpu[0].preempted = 2;
    assert!(
        broken.validate().is_err(),
        "an off-CPU interval of no cause"
    );
    let mut broken = valid.clone();
    broken.gaps[0].end = (-1).into();
    assert!(broken.validate().is_err(), "a gap ending before it begins");
    let mut broken = valid.clone();
    broken.frames[0].module = 9.into();
    assert!(broken.validate().is_err(), "a frame of no module");
    let mut broken = valid.clone();
    broken.zones[0].end = (-1).into();
    assert!(broken.validate().is_err(), "a zone ending before it starts");
    let mut broken = valid;
    broken.strings.pop();
    assert!(broken.validate().is_err(), "an unterminated string pool");
}

/// The capture fuzzer's seed is a whole file of this revision, so that
/// the fuzzer starts past validation. `USCOPE_UPDATE_SEED=1` rewrites it.
#[test]
fn the_fuzzers_seed_is_a_capture_of_this_revision() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/capture/two-threads");
    let mut expected = Vec::new();
    file::write(&sample_capture(0), &mut expected).expect("write");
    if std::env::var_os("USCOPE_UPDATE_SEED").is_some() {
        std::fs::write(&path, &expected).expect("write the seed");
    }
    let seed = std::fs::read(&path).expect("read the seed");
    assert!(
        seed == expected,
        "the seed is stale: rerun with USCOPE_UPDATE_SEED=1"
    );
}
