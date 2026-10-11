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
        }],
    );
    let deep = builder.stack(&[work, main]);
    let shallow = builder.stack(&[main]);
    let broken = builder.stack(&[work, cut]);
    builder.sample(main_thread, offset + 5, 1000, deep, NONE);
    builder.sample(main_thread, offset + 2, 1000, shallow, NONE);
    builder.sample(worker, offset + 3, 1000, broken, NONE);
    builder.count(AccountKind::LostSamples as u32, 2);
    let site = builder.site("handle", "app.rs", 9, module, 1);
    builder.zone(
        worker,
        0,
        Zone {
            start: (offset + 3).into(),
            end: (offset + 9).into(),
            site: site.into(),
            flags: 0.into(),
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
    broken.frames[0].module = 9.into();
    assert!(broken.validate().is_err(), "a frame of no module");
    let mut broken = valid.clone();
    broken.zones[0].end = (-1).into();
    assert!(broken.validate().is_err(), "a zone ending before it starts");
    let mut broken = valid;
    broken.strings.pop();
    assert!(broken.validate().is_err(), "an unterminated string pool");
}
