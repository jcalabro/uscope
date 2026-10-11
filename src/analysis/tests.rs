use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::capture::Capture;
use crate::capture::builder::{CaptureBuilder, LineInput};
use crate::capture::records::{FrameKind, NONE, Truncation};

const FUNCTIONS: [&str; 5] = ["a", "b", "c", "d", "e"];

/// Goroutines 1 to 3 by choice, the second labeled, and this choice for
/// none.
const UNNAMED: u32 = 3;

fn task_row(choice: u32) -> u32 {
    if choice == UNNAMED { NONE } else { choice }
}

/// How a query by task names a task by its choice.
fn group_name(choice: u32) -> String {
    match choice {
        UNNAMED => "[no task known]".to_owned(),
        1 => "goroutine 2 {phase=spin}".to_owned(),
        _ => format!("goroutine {}", choice + 1),
    }
}

/// A capture of `stacks` (frame choices, innermost first, and whether a
/// truncation ends them), `samples` (thread, time, weight, stack, task
/// choice), and `waits` (time, weight, stack, task choice). Frame 5 has no
/// symbols, and frames 6 and 7 inline one function into another.
fn capture(
    stacks: &[(Vec<usize>, bool)],
    samples: &[(u32, i64, u64, usize, u32)],
    waits: &[(i64, u64, usize, u32)],
) -> Capture {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let mut frames = Vec::new();
    for (index, name) in FUNCTIONS.iter().enumerate() {
        let frame = builder.frame(FrameKind::Native, module, 0x10 * index as u64, 0);
        builder.symbolize(frame, module, &[line(name)]);
        frames.push(frame);
    }
    frames.push(builder.frame(FrameKind::Native, module, 0x500, 0));
    for (address, (inner, outer)) in [(0x600, ("b", "a")), (0x700, ("a", "c"))] {
        let frame = builder.frame(FrameKind::Native, module, address, 0);
        builder.symbolize(frame, module, &[line(inner), line(outer)]);
        frames.push(frame);
    }
    let truncation = builder.truncation(Truncation::FramePointerNotKept, NONE);
    let stacks: Vec<u32> = stacks
        .iter()
        .map(|(choices, truncated)| {
            let mut stack: Vec<u32> = choices.iter().map(|choice| frames[*choice]).collect();
            if *truncated {
                stack.push(truncation);
            }
            builder.stack(&stack)
        })
        .collect();
    let threads = [
        builder.thread(1, 1, 0),
        builder.thread(1, 2, 0),
        builder.thread(1, 3, 0),
    ];
    let tasks: Vec<u32> = (1..=3)
        .map(|number| builder.task("goroutine", number, 0))
        .collect();
    builder.describe_task(tasks[1], NONE, Some("phase=spin"));
    for &(thread, time, weight, stack, task) in samples {
        builder.sample(
            threads[thread as usize],
            time,
            weight,
            stacks[stack],
            task_row(task),
        );
    }
    for &(time, weight, stack, task) in waits {
        builder.task_wait(task_row(task), time, weight, stacks[stack], "chan receive");
    }
    builder.finish()
}

fn line(function: &str) -> LineInput<'_> {
    LineInput {
        function,
        file: Some("app.c"),
        line: 1,
        column: 0,
        declared: None,
        role: 0,
    }
}

/// A transform of a function, by its name.
#[derive(Debug, Clone)]
enum Named {
    Focus(&'static str),
    Merge(&'static str),
    Drop(&'static str),
    Collapse(&'static str),
}

/// The names of a sample's path, computed from the capture's tables alone.
fn naive_path(
    capture: &Capture,
    stack: u32,
    invert: bool,
    transforms: &[Named],
) -> Option<Vec<String>> {
    let mut path = Vec::new();
    for frame in capture.stack_frames(stack) {
        let record = &capture.frames[frame as usize];
        if record.kind == FrameKind::Truncated as u8 {
            path.push("[frame pointer not kept]".to_owned());
            continue;
        }
        let lines = capture.frame_lines(frame);
        if lines.is_empty() {
            path.push(format!("app+{:#x}", record.address.get()));
        }
        for line in lines {
            let function = &capture.functions[line.function.get() as usize];
            path.push(capture.string(function.name.get()).to_owned());
        }
    }
    if !invert {
        path.reverse();
    }
    for transform in transforms {
        match transform {
            Named::Focus(name) => {
                let first = path.iter().position(|step| step == name)?;
                path = path.split_off(first);
            }
            Named::Merge(name) => path.retain(|step| step != name),
            Named::Drop(name) => {
                if path.iter().any(|step| step == name) {
                    return None;
                }
            }
            Named::Collapse(name) => {
                let mut collapsed: Vec<String> = Vec::new();
                for step in path {
                    if !(step == *name && collapsed.last() == Some(&step)) {
                        collapsed.push(step);
                    }
                }
                path = collapsed;
            }
        }
    }
    Some(path)
}

fn key_named(keys: &Keys<'_>, name: &str) -> Option<u32> {
    (0..index(keys.count())).find(|key| keys.name(*key) == name)
}

/// Stacks, samples, and waits as [`capture`] takes them.
type Stacks = Vec<(Vec<usize>, bool)>;
type Samples = Vec<(u32, i64, u64, usize, u32)>;
type Waits = Vec<(i64, u64, usize, u32)>;
/// A range, threads, transforms by kind and function, and inversion; and
/// whether waits are queried, tasks by choice, and whether by task.
type Selection = (
    (Option<(i64, i64)>, Option<Vec<u32>>, Vec<(u8, usize)>, bool),
    (bool, Option<Vec<u32>>, bool),
);

fn strategy() -> impl Strategy<Value = (Stacks, (Samples, Waits), Selection)> {
    let stacks = prop::collection::vec(
        (prop::collection::vec(0..8_usize, 1..7), any::<bool>()),
        1..6,
    );
    stacks.prop_flat_map(|stacks| {
        let count = stacks.len();
        let samples = prop::collection::vec(
            (0..3_u32, 0..100_i64, 1..5_u64, 0..count, 0..=UNNAMED),
            0..40,
        );
        let waits = prop::collection::vec((0..100_i64, 1..5_u64, 0..count, 0..UNNAMED), 0..10);
        let range = prop::option::of((0..100_i64, 0..100_i64));
        let threads = prop::option::of(prop::collection::vec(0..3_u32, 0..3));
        let transforms = prop::collection::vec((0..4_u8, 0..FUNCTIONS.len()), 0..3);
        let tasks = prop::option::of(prop::collection::vec(0..=UNNAMED, 0..3));
        (
            Just(stacks),
            (samples, waits),
            (
                (range, threads, transforms, any::<bool>()),
                (any::<bool>(), tasks, any::<bool>()),
            ),
        )
    })
}

/// What the oracle follows: each selected sample's or wait's task, stack,
/// and weight.
fn selected(
    capture: &Capture,
    range: Option<(i64, i64)>,
    threads: Option<&Vec<u32>>,
    (waits, tasks): (bool, Option<&Vec<u32>>),
) -> Vec<(u32, u32, u64)> {
    let in_range = |time: i64| range.is_none_or(|(start, end)| start <= time && time < end);
    let task_kept =
        |task: u32| tasks.is_none_or(|tasks| tasks.iter().any(|choice| task_row(*choice) == task));
    let mut chosen = Vec::new();
    if waits {
        for wait in &capture.task_waits {
            if in_range(wait.time.get()) && task_kept(wait.task.get()) {
                chosen.push((wait.task.get(), wait.stack.get(), wait.weight.get()));
            }
        }
        return chosen;
    }
    for range_row in &capture.sample_ranges {
        if threads.is_some_and(|threads| !threads.contains(&range_row.thread.get())) {
            continue;
        }
        for sample in capture.thread_samples(range_row) {
            if in_range(sample.time.get()) && task_kept(sample.task.get()) {
                chosen.push((sample.task.get(), sample.stack.get(), sample.weight.get()));
            }
        }
    }
    chosen
}

proptest! {
    /// The engine agrees with a naive oracle that follows each sample
    /// alone: the same weight reaches every path of the tree and every
    /// function, under any selection and transforms, inverted or not.
    #[test]
    fn aggregation_agrees_with_a_sample_by_sample_oracle(
        (stacks, (samples, waits), ((range, threads, transforms, invert), (of_waits, tasks, by_task))) in strategy()
    ) {
        let capture = capture(&stacks, &samples, &waits);
        prop_assert_eq!(capture.validate(), Ok(()));
        let keys = Keys::new(&capture, Granularity::Function);
        let named: Vec<Named> = transforms
            .iter()
            .map(|(kind, function)| {
                let name = FUNCTIONS[*function];
                match kind {
                    0 => Named::Focus(name),
                    1 => Named::Merge(name),
                    2 => Named::Drop(name),
                    _ => Named::Collapse(name),
                }
            })
            .collect();
        // A transform of a function no frame has removes nothing, or, as a
        // focus, everything; the engine can only name functions it has.
        let known = |name: &str| key_named(&keys, name);
        let mut query = Query {
            source: if of_waits { Source::Waits } else { Source::Samples },
            range,
            threads: threads.clone(),
            tasks: tasks.as_ref().map(|tasks| tasks.iter().map(|choice| task_row(*choice)).collect()),
            invert,
            by_task,
            ..Query::default()
        };
        let mut naive_transforms = Vec::new();
        for transform in &named {
            let (Named::Focus(name) | Named::Merge(name) | Named::Drop(name) | Named::Collapse(name)) = transform;
            let Some(key) = known(name) else { continue };
            naive_transforms.push(transform.clone());
            query.transforms.push(match transform {
                Named::Focus(_) => Transform::Focus(key),
                Named::Merge(_) => Transform::Merge(key),
                Named::Drop(_) => Transform::Drop(key),
                Named::Collapse(_) => Transform::CollapseRecursion(key),
            });
        }

        // The oracle: each selected sample alone.
        let mut expected_totals: BTreeMap<Vec<String>, u64> = BTreeMap::new();
        let mut expected_functions: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut expected_weight = 0;
        for (task, stack, weight) in selected(&capture, range, threads.as_ref(), (of_waits, tasks.as_ref())) {
            {
                let Some(mut path) = naive_path(&capture, stack, invert, &naive_transforms) else {
                    continue;
                };
                if by_task {
                    let choice = if task == NONE { UNNAMED } else { task };
                    if invert {
                        path.push(group_name(choice));
                    } else {
                        path.insert(0, group_name(choice));
                    }
                }
                expected_weight += weight;
                for depth in 1..=path.len() {
                    *expected_totals.entry(path[..depth].to_vec()).or_default() += weight;
                }
                let innermost = if invert { path.first() } else { path.last() };
                if let Some(innermost) = innermost {
                    expected_functions.entry(innermost.clone()).or_default().0 += weight;
                }
                let mut seen: Vec<&String> = Vec::new();
                for step in &path {
                    if !seen.contains(&step) {
                        seen.push(step);
                        expected_functions.entry(step.clone()).or_default().1 += weight;
                    }
                }
            }
        }

        let tree = aggregate(&capture, &keys, &query);
        prop_assert_eq!(tree.weight, expected_weight);
        let mut totals: BTreeMap<Vec<String>, u64> = BTreeMap::new();
        for node in 0..tree.len() {
            let mut path = Vec::new();
            let mut at = index(node);
            while at != NONE {
                path.push(keys.name(tree.key[at as usize]));
                at = tree.parent[at as usize];
            }
            path.reverse();
            totals.insert(path, tree.total[node]);
            // A node's subtree is the run of deeper nodes after it.
            let end = tree.end[node] as usize;
            prop_assert!(end > node && end <= tree.len());
            prop_assert!((node + 1..end).all(|inner| tree.depth[inner] > tree.depth[node]));
            prop_assert!(end == tree.len() || tree.depth[end] <= tree.depth[node]);
        }
        prop_assert_eq!(totals, expected_totals);

        let costs = functions(&capture, &keys, &query);
        prop_assert_eq!(costs.weight, expected_weight);
        let costs: BTreeMap<String, (u64, u64)> = costs
            .rows
            .into_iter()
            .map(|cost| (keys.name(cost.key), (cost.own, cost.total)))
            .collect();
        let expected_functions: BTreeMap<String, (u64, u64)> = expected_functions
            .into_iter()
            .filter(|(_, (_, total))| *total > 0)
            .collect();
        prop_assert_eq!(costs, expected_functions);
    }
}

#[test]
fn density_buckets_kept_samples_by_lane_and_time() {
    let capture = capture(
        &[(vec![0], false), (vec![1], false)],
        &[
            (0, 0, 1, 0, 0),
            (0, 9, 2, 0, UNNAMED),
            (1, 5, 4, 1, 0),
            (1, 10, 8, 1, 0),
        ],
        &[(3, 7, 0, 1)],
    );
    let keys = Keys::new(&capture, Granularity::Function);
    let all = Query {
        range: Some((0, 10)),
        ..Query::default()
    };
    // The sample at 10 lies outside [0, 10).
    assert_eq!(
        density(&capture, &keys, &all, 2, Lanes::Threads),
        [(0, vec![1, 2]), (1, vec![0, 4])]
    );
    assert_eq!(
        density(&capture, &keys, &all, 2, Lanes::Tasks),
        [(0, vec![1, 4]), (NONE, vec![0, 2])]
    );
    let waits = Query {
        source: Source::Waits,
        ..all.clone()
    };
    assert_eq!(
        density(&capture, &keys, &waits, 2, Lanes::Threads),
        [(1, vec![7, 0])]
    );
    let without_b = Query {
        transforms: vec![Transform::Drop(key_named(&keys, "b").expect("b"))],
        ..all
    };
    assert_eq!(
        density(&capture, &keys, &without_b, 2, Lanes::Threads),
        [(0, vec![1, 2])]
    );
}

#[test]
fn off_cpu_time_falls_in_each_slice_it_overlaps() {
    let mut builder = CaptureBuilder::new();
    let (a, b) = (builder.thread(1, 1, 0), builder.thread(1, 2, 0));
    builder.off_cpu(a, 5, 20, false);
    builder.off_cpu(a, 30, 4, true);
    builder.off_cpu(a, 45, 10, false);
    builder.off_cpu(b, -5, 100, false);
    let capture = builder.finish();
    let query = Query {
        range: Some((0, 40)),
        ..Query::default()
    };
    // Slices of ten from 0; b was off from before the range to after it.
    assert_eq!(
        off_cpu(&capture, &query, 4),
        [
            OffCpuLane {
                thread: a,
                blocked: vec![5, 10, 5, 0],
                preempted: vec![0, 0, 0, 4],
            },
            OffCpuLane {
                thread: b,
                blocked: vec![10; 4],
                preempted: vec![0; 4],
            },
        ]
    );
    // Slices of an uneven range share it out without losing any.
    let uneven = Query {
        range: Some((0, 10)),
        threads: Some(vec![b]),
        ..Query::default()
    };
    let lanes = off_cpu(&capture, &uneven, 3);
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].blocked.iter().sum::<u64>(), 10, "{lanes:?}");
}

/// The analysis is pure: it reads captures and nothing else, so a query
/// answers the same wherever it runs. Tests are exempt.
#[test]
fn the_analysis_stays_pure() {
    const FORBIDDEN: [&str; 11] = [
        "crate::backend",
        "crate::debug_info",
        "crate::profiler",
        "crate::sim",
        "nix::",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/analysis");
    let mut checked = 0;
    for entry in std::fs::read_dir(&root).expect("read the analysis's sources") {
        let path = entry.expect("a directory entry").path();
        if path.file_name().is_some_and(|name| name == "tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read a source file");
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();
        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 3, "the analysis's sources were found");
}

/// Each path of the tree that samples end at, outermost first, with the
/// weight that ends there.
fn ending_paths(tree: &CallTree, keys: &Keys<'_>) -> BTreeMap<Vec<String>, u64> {
    (0..tree.len())
        .filter(|node| tree.own[*node] > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = index(node);
            while at != NONE {
                path.push(keys.name(tree.key[at as usize]));
                at = tree.parent[at as usize];
            }
            path.reverse();
            (path, tree.own[node])
        })
        .collect()
}

#[test]
fn a_logical_stack_is_a_tasks_own_under_its_dispatch() {
    use crate::CodeRole::{Dispatch, Ordinary, Outermost, RuntimeInternal, Wrapper};
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/server", None, [0; 16], 0);
    let mut frame = |name: &str, role: crate::CodeRole| {
        let address = 0x100 * (builder.frames().len() as u64 + 1);
        let frame = builder.frame(FrameKind::Native, module, address, 0);
        builder.symbolize(
            frame,
            module,
            &[LineInput {
                role: crate::capture::records::role_number(role),
                ..line(name)
            }],
        );
        frame
    };
    let leaf = frame("decode", Ordinary);
    let pinned = frame("<Pin<Box<F>> as Future>::poll", Wrapper);
    let handler = frame("handler::{closure#0}", Ordinary);
    let core = frame("Core::poll", RuntimeInternal);
    let harness = frame("Harness::poll", Dispatch);
    let run = frame("run_task", RuntimeInternal);
    let start = frame("thread_start", Outermost);
    let park = frame("park", RuntimeInternal);
    let coop = frame("coop::budget", RuntimeInternal);
    let stacks = [
        builder.stack(&[leaf, pinned, handler, core, harness, run, start]),
        builder.stack(&[park, run, start]),
        builder.stack(&[coop, harness, run, start]),
    ];
    let thread = builder.thread(1, 1, 0);
    for (time, stack) in stacks.into_iter().enumerate() {
        builder.sample(
            thread,
            i64::try_from(time).expect("a time"),
            1 << time,
            stack,
            NONE,
        );
    }
    let capture = builder.finish();
    let keys = Keys::new(&capture, Granularity::Function);
    let names = |path: &[&str]| {
        path.iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };

    let logical = aggregate(
        &capture,
        &keys,
        &Query {
            logical: true,
            ..Query::default()
        },
    );
    assert_eq!(
        ending_paths(&logical, &keys),
        BTreeMap::from([
            (
                names(&[
                    "Harness::poll",
                    "[task] handler::{closure#0}",
                    "handler::{closure#0}",
                    "decode"
                ]),
                1
            ),
            (names(&["thread_start", "run_task", "park"]), 2),
            (names(&["Harness::poll", "coop::budget"]), 4),
        ])
    );
    // The native stacks are one switch away.
    let native = aggregate(&capture, &keys, &Query::default());
    assert_eq!(
        ending_paths(&native, &keys)[&names(&[
            "thread_start",
            "run_task",
            "Harness::poll",
            "Core::poll",
            "handler::{closure#0}",
            "<Pin<Box<F>> as Future>::poll",
            "decode",
        ])],
        1
    );
}
