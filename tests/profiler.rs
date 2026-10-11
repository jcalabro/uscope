mod support;

use std::process::{Command, Stdio};

use support::Scenario;
use uscope::profiler::{
    CallNode, DEEP_STACK_COPY, Preset, RecordOptions, SMALL_STACK_COPY, SampleSource, SampledStack,
    StackCopy, StackEnding, StackView, Watch, record, record_watched,
};

/// The profile-stacks fixture across languages, compilers, optimization,
/// frame pointers, PIE, and linkers, and whether the build keeps frame
/// pointers.
const VARIANTS: [(&str, bool); 4] = [
    ("gcc-o0", true),
    ("gcc-o2-nofp", false),
    ("clang-o2", true),
    ("clang-o2-nofp", false),
];

/// The same chains in C++, in Rust, and linked by mold, with the names of
/// the fixture's middle and outer functions and of its main.
const OTHER_VARIANTS: [(&str, [&str; 3]); 5] = [
    ("gcc-mold", ["middle", "outer", "descend"]),
    (
        "cpp-gcc-o2-nofp",
        ["stacks::middle", "stacks::outer", "main"],
    ),
    ("cpp-clang-o0", ["stacks::middle", "stacks::outer", "main"]),
    (
        "rust-o0",
        [
            "profile_stacks::middle",
            "profile_stacks::outer",
            "profile_stacks::main",
        ],
    ),
    (
        "rust-o2-nofp",
        [
            "profile_stacks::middle",
            "profile_stacks::outer",
            "profile_stacks::main",
        ],
    ),
];

fn command(variant: &str, iterations: u64) -> Command {
    let mut command = Command::new(Scenario::fixture(&format!("profile-stacks-{variant}")));
    command
        .arg(iterations.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Options that sample each call of the function `symbol` names.
fn calls_of(symbol: &str) -> RecordOptions {
    RecordOptions {
        source: SampleSource::Function {
            symbol: symbol.into(),
        },
        ..RecordOptions::default()
    }
}

/// The stack of each call of `leaf` on each thread, innermost first.
fn expected(stack: &SampledStack) -> Vec<&'static str> {
    let mut expected = vec!["leaf", "middle", "outer"];
    if stack.functions.get(3).map(String::as_str) == Some("worker") {
        expected.extend(["worker", "start_thread", "__GI___clone3"]);
    } else {
        expected.extend(["descend"; 5]);
        expected.extend([
            "main",
            "__libc_start_call_main",
            "__libc_start_main",
            "_start",
        ]);
    }
    expected
}

#[test]
fn a_function_breakpoint_records_its_exact_callers_in_every_build() {
    for (variant, frame_pointers) in VARIANTS {
        for copy in [DEEP_STACK_COPY, SMALL_STACK_COPY] {
            let options = RecordOptions {
                stack: StackCopy::Bytes(copy),
                ..calls_of("leaf")
            };
            let recording = record(command(variant, 1), &options).expect("record");
            assert!(recording.status.success());
            let stacks = recording.profile.stacks();
            assert_eq!(stacks.len(), 6, "{variant} {copy}: every call is a sample");
            for stack in &stacks {
                let expected = expected(stack);
                let context = format!("{variant} {copy}: {stack:?}");
                // A stack is never wrong: what it has is exact, and where
                // it stops short it says why.
                match &stack.ending {
                    StackEnding::Complete => assert_eq!(stack.functions, expected, "{context}"),
                    StackEnding::Truncated(reason) => {
                        assert!(
                            stack.functions.len() <= expected.len()
                                && stack.functions.iter().zip(&expected).all(|(a, b)| a == b),
                            "{context}: not a prefix"
                        );
                        assert_eq!(reason, "stack copy exhausted", "{context}");
                    }
                }
                // A deep copy unwinds everything, and a small one does too
                // where the kernel's chain continues it.
                if copy == DEEP_STACK_COPY || frame_pointers {
                    assert_eq!(stack.ending, StackEnding::Complete, "{context}");
                } else if expected.contains(&"main") {
                    // The main thread's frames outgrow a small copy, so the
                    // frame-pointer builds complete by the kernel's chain.
                    assert_ne!(stack.ending, StackEnding::Complete, "{context}");
                }
            }
        }
    }
}

#[test]
fn auto_mode_copies_a_small_stack_only_where_the_chain_carries_every_stack() {
    for (variant, bytes, why) in [
        (
            "musl",
            SMALL_STACK_COPY,
            "auto: the program keeps the frame-pointer chain where it calls",
        ),
        (
            "musl-nofp",
            DEEP_STACK_COPY,
            "auto: main calls without keeping the frame-pointer chain",
        ),
        (
            "gcc-o0",
            DEEP_STACK_COPY,
            "auto: the program loads libraries, which are known only once it runs",
        ),
    ] {
        let options = RecordOptions {
            stack: StackCopy::Auto,
            ..calls_of("leaf")
        };
        let recording = record(command(variant, 1), &options).expect("record");
        assert!(recording.status.success());
        let profile = &recording.profile;
        assert_eq!(
            profile.setting("stack bytes"),
            Some(bytes.to_string().as_str()),
            "{variant}"
        );
        assert_eq!(profile.setting("stack copy"), Some(why), "{variant}");
        // The copy auto mode chose unwinds what the deepest does.
        let deep = RecordOptions {
            stack: StackCopy::Bytes(DEEP_STACK_COPY),
            ..options
        };
        let deep = record(command(variant, 1), &deep).expect("record");
        let shapes = |stacks: Vec<SampledStack>| {
            let mut shapes: Vec<_> = stacks
                .into_iter()
                .map(|stack| (stack.functions, stack.ending))
                .collect();
            shapes.sort();
            shapes
        };
        let stacks = shapes(profile.stacks());
        assert_eq!(stacks.len(), 6, "{variant}: every call is a sample");
        assert_eq!(stacks, shapes(deep.profile.stacks()), "{variant}");
        for (functions, _) in &stacks {
            assert_eq!(functions[..3], ["leaf", "middle", "outer"], "{variant}");
        }
    }
}

/// Whether `functions`, innermost first, is a call chain of the fixture's
/// own code that its source allows: a suffix of a leaf call's stack.
fn fixture_chain(functions: &[String]) -> Option<bool> {
    let ours = ["leaf", "middle", "outer", "descend", "worker", "main"];
    if !ours.contains(&functions.first()?.as_str()) {
        return None;
    }
    let names: Vec<&str> = functions.iter().map(String::as_str).collect();
    let main = [
        "main",
        "__libc_start_call_main",
        "__libc_start_main",
        "_start",
    ];
    let worker = ["worker", "start_thread", "__GI___clone3"];
    let mut rest = &names[..];
    for prefix in ["leaf", "middle"] {
        if rest.first() == Some(&prefix) {
            rest = &rest[1..];
        }
    }
    if rest.first() == Some(&"outer") {
        rest = &rest[1..];
        if rest.first() == Some(&"worker") {
            return Some(worker.starts_with(rest));
        }
        // From outer, every frame of the descent.
        if !rest.starts_with(&["descend"; 5]) {
            return Some(false);
        }
    }
    let descents = rest.iter().take_while(|name| **name == "descend").count();
    Some(descents <= 5 && main.starts_with(&rest[descents..]))
}

#[test]
fn function_breakpoints_name_every_language_and_layout_exactly() {
    for (variant, [middle, outer, main]) in OTHER_VARIANTS {
        let options = calls_of("leaf");
        let recording = record(command(variant, 1), &options).expect("record");
        assert!(recording.status.success());
        let stacks = recording.profile.stacks();
        assert_eq!(stacks.len(), 6, "{variant}: every call is a sample");
        let mut main_thread = 0;
        for stack in &stacks {
            let context = format!("{variant}: {stack:?}");
            // Code inlined into leaf's first instruction is shown inside it.
            let leaf = stack.functions.iter().position(|name| name == "leaf");
            let own = &stack.functions[leaf.expect(&context)..];
            assert_eq!(own[..3], ["leaf", middle, outer], "{context}");
            main_thread += usize::from(own[3..].iter().any(|name| name == main));
            assert_eq!(stack.ending, StackEnding::Complete, "{context}");
            let outermost = stack.functions.last().map(String::as_str);
            assert!(
                matches!(outermost, Some("_start" | "__GI___clone3")),
                "{context}"
            );
        }
        assert_eq!(
            main_thread, 3,
            "{variant}: the main thread's calls name {main}"
        );
    }
}

/// Each path of a call tree that samples end at, outermost first, with
/// the weight that ends there.
fn ending_paths(tree: &[CallNode]) -> Vec<(Vec<&str>, u64)> {
    (0..tree.len())
        .filter(|node| tree[*node].own > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = Some(node);
            while let Some(node) = at {
                path.push(tree[node].name.as_str());
                at = tree[node].parent;
            }
            path.reverse();
            (path, tree[node].own)
        })
        .collect()
}

#[test]
fn an_async_tasks_samples_show_its_own_stack_of_awaits() {
    for variant in ["o0", "o3"] {
        let mut command = Command::new(Scenario::fixture(&format!("tokio-profile-{variant}")));
        command.stdout(Stdio::null());
        // Rings too small for more than a few samples lose most calls.
        let options = RecordOptions {
            ring_pages: 1,
            ..calls_of("profile::work")
        };
        let recording = record(command, &options).expect("record");
        assert!(recording.status.success());
        let profile = &recording.profile;
        // Four tasks call work five times each, and every call a full ring
        // drops is counted.
        let recorded = profile.total_weight();
        assert!(recorded > 0, "{variant}");
        assert_eq!(recorded + profile.lost_samples(), 20, "{variant}");

        let logical_tree = profile.call_tree(StackView::Logical);
        let logical = ending_paths(&logical_tree);
        assert_eq!(logical.len(), 1, "{variant}: {logical:?}");
        let (path, weight) = &logical[0];
        assert_eq!(*weight, recorded);
        let context = format!("{variant}: {path:?}");
        assert!(
            path[0]
                .starts_with("<tokio::runtime::task::harness::Harness<profile::task::{closure#0}"),
            "{context}"
        );
        let work = path[1..].iter().position(|name| *name == "profile::work");
        let (own, inlined) = path[1..].split_at(work.expect(&context));
        assert_eq!(
            own,
            [
                "[task] profile::task::{closure#0}",
                "profile::task::{closure#0}",
                "profile::outer::{closure#0}",
                "profile::inner::{closure#0}",
            ],
            "{context}"
        );
        // Code inlined into work's first instruction is shown inside it.
        assert!(
            inlined[1..]
                .iter()
                .all(|name| name.starts_with("core::hint::")),
            "{context}"
        );

        // The threads' own frames are one switch away.
        let native_tree = profile.call_tree(StackView::Native);
        let native = ending_paths(&native_tree);
        assert!(
            native.iter().all(|(path, _)| path[0] == "__GI___clone3"),
            "{variant}: {native:?}"
        );
    }
}

#[test]
fn a_watched_recording_publishes_as_it_goes_and_stops_when_asked() {
    use std::os::unix::process::ExitStatusExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let stop = Arc::new(AtomicBool::new(false));
    let (published, snapshots) = std::sync::mpsc::channel();
    let recording = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let mut command = Command::new(Scenario::fixture("spin"));
            command.stdout(Stdio::null());
            record_watched(
                command,
                &RecordOptions::default(),
                &mut Watch {
                    every: std::time::Duration::ZERO,
                    publish: &mut |profile| {
                        let pid = profile.setting("pid").map(str::to_owned);
                        let _ = published.send((profile.samples(), pid));
                    },
                    stop: &stop,
                },
            )
        }
    });
    // The first profile is published as the program starts, empty.
    let deadline = std::time::Duration::from_secs(10);
    let (first, pid) = snapshots.recv_timeout(deadline).expect("the first profile");
    assert_eq!(first, 0);
    let pid = pid.expect("the program's pid");
    let mut seen = 0;
    while seen == 0 {
        seen = snapshots
            .recv_timeout(deadline)
            .expect("a profile with samples")
            .0;
    }
    stop.store(true, Ordering::Relaxed);
    let recording = recording.join().expect("the recording").expect("record");
    assert_eq!(recording.status.signal(), Some(9), "{:?}", recording.status);
    assert!(recording.profile.samples() >= seen);
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the program is gone"
    );
}

#[test]
fn clock_samples_are_valid_stacks_and_survive_a_file() {
    let scratch = support::ScratchDir::new("profiler-clock");
    for (variant, frame_pointers) in VARIANTS {
        let mut options = RecordOptions::preset(Preset::Maximum);
        // Deep enough for the fixture's stacks, and small enough that a
        // loaded machine drops few samples.
        options.stack = StackCopy::Bytes(if frame_pointers { 512 } else { 4096 });
        let recording = record(command(variant, 2_000_000), &options).expect("record");
        assert!(recording.status.success());
        let profile = recording.profile;
        let stacks = profile.stacks();
        let mut ours = 0;
        for stack in &stacks {
            let context = format!("{variant}: {stack:?}");
            if let Some(valid) = fixture_chain(&stack.functions) {
                ours += 1;
                assert!(valid, "{context}");
            }
            if stack.ending == StackEnding::Complete {
                let outermost = stack.functions.last().map(String::as_str);
                assert!(
                    matches!(outermost, Some("_start" | "__GI___clone3")),
                    "{context}"
                );
            }
        }
        // A loaded machine may drop most samples, which the profile
        // counts; the property needs only some of the fixture's own.
        assert!(
            ours > 0,
            "{variant}: none of {} samples in the fixture, {} lost",
            stacks.len(),
            profile.lost_samples()
        );

        let path = scratch.path().join(format!("{variant}.usprof"));
        profile.write(&path).expect("write");
        let read = uscope::profiler::Profile::read(&path).expect("read");
        assert!(!read.truncated);
        assert_eq!(
            read.profile, profile,
            "{variant}: the file holds the whole profile"
        );
    }
}

#[test]
fn a_program_that_cannot_start_fails_the_recording() {
    let missing = Command::new("/nonexistent/program");
    let error = record(missing, &RecordOptions::default()).expect_err("no program");
    assert!(
        matches!(&error, uscope::profiler::RecordError::Start(error) if error.kind() == std::io::ErrorKind::NotFound),
        "{error:?}"
    );
}

#[test]
fn a_forked_child_is_sampled_through_the_mappings_it_inherits() {
    let options = calls_of("shared_work");
    let mut command = Command::new(Scenario::fixture("fork"));
    command.stdout(Stdio::null());
    let recording = record(command, &options).expect("record");
    assert!(recording.status.success());
    let stacks = recording.profile.stacks();
    let pids: std::collections::BTreeSet<u32> = stacks.iter().map(|stack| stack.pid).collect();
    assert_eq!(pids.len(), 2, "the parent and its child: {stacks:?}");
    for stack in &stacks {
        assert_eq!(
            stack.functions,
            [
                "shared_work",
                "main",
                "__libc_start_call_main",
                "__libc_start_main",
                "_start"
            ],
            "{stack:?}"
        );
        assert_eq!(stack.ending, StackEnding::Complete);
    }
}

#[test]
fn a_program_killed_by_a_signal_keeps_its_profile() {
    use std::os::unix::process::ExitStatusExt;
    let options = calls_of("fault");
    let recording =
        record(Command::new(Scenario::fixture("fatal-signal")), &options).expect("record");
    assert_eq!(recording.status.signal(), Some(nix::libc::SIGSEGV));
    let stacks = recording.profile.stacks();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].functions[..2], ["fault", "main"]);
}
