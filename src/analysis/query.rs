//! Aggregating a selection of samples into a call tree, function costs,
//! and densities over time.
//!
//! A query selects samples, or the waits task snapshots found, by time,
//! thread, and task, turns each one's stack into a path of keys, outermost
//! first or, inverted, innermost first, and reshapes the path by its
//! transforms in order. A logical query first shows each async task's
//! frames as a stack of their own, and a query by task roots each path at
//! its task. Samples are counted per distinct stack first, and task, so
//! the work is linear in the samples selected and the stacks they use.

use foldhash::HashMap;

use super::{Keys, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::NONE;

/// What a query aggregates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    /// CPU samples, or a breakpoint's.
    #[default]
    Samples,
    /// Where task snapshots found tasks waiting, each weighted by the time
    /// it stands for.
    Waits,
}

/// Which samples, seen how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub source: Source,
    /// Samples taken in `[start, end)`, or all.
    pub range: Option<(i64, i64)>,
    /// Samples of these threads, by their rows in the capture, or of all.
    /// Waits are of no thread, so this selects samples only.
    pub threads: Option<Vec<u32>>,
    /// Samples or waits of these tasks, by their rows, or of any: [`NONE`]
    /// is a sample no task snapshot named.
    pub tasks: Option<Vec<u32>>,
    pub transforms: Vec<Transform>,
    /// Paths from the innermost frame out, so that the tree is of callers.
    pub invert: bool,
    /// Each async task's frames as a stack of their own (see [`logical`]),
    /// before any transform.
    pub logical: bool,
    /// Roots each path, after its transforms, at the task it ran or waited
    /// in.
    pub by_task: bool,
}

/// One step of reshaping each sample's path, applied in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// Keeps samples whose path has the key, from its first occurrence on:
    /// the key becomes the root, and its calls from everywhere merge.
    Focus(u32),
    /// Removes the key from every path, giving its cost to its caller.
    Merge(u32),
    /// Removes every sample whose path has the key.
    Drop(u32),
    /// Merges each run of the key calling itself into its first call.
    CollapseRecursion(u32),
}

/// The call tree of a query, its nodes in depth-first order with each
/// node's children by total, largest first; a node's subtree is the
/// nodes from it to its `end`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallTree {
    pub key: Vec<u32>,
    /// The caller's node, or [`NONE`] for a root.
    pub parent: Vec<u32>,
    pub depth: Vec<u32>,
    /// The weight of samples whose path ends at the node.
    pub own: Vec<u64>,
    /// The weight of samples whose path passes through the node.
    pub total: Vec<u64>,
    /// One past the node's last descendant.
    pub end: Vec<u32>,
    /// The weight of every sample selected and kept.
    pub weight: u64,
    pub samples: u64,
}

impl CallTree {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.key.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.key.is_empty()
    }
}

/// What one key costs under a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub key: u32,
    /// Weight of samples whose path ends with it.
    pub own: u64,
    /// Weight of samples whose path has it, counted once per sample.
    pub total: u64,
}

/// One sample or wait a query selects.
#[derive(Debug, Clone, Copy)]
struct Selected {
    time: i64,
    /// Its thread's row, or [`NONE`] for a wait.
    thread: u32,
    task: u32,
    stack: u32,
    weight: u64,
}

/// Each sample or wait `query` selects, in time order per thread, but for
/// its transforms.
fn each(capture: &Capture, query: &Query, mut take: impl FnMut(Selected)) {
    let task_kept = |task: u32| {
        query
            .tasks
            .as_ref()
            .is_none_or(|tasks| tasks.contains(&task))
    };
    match query.source {
        Source::Samples => {
            for range in &capture.sample_ranges {
                let thread = range.thread.get();
                if query
                    .threads
                    .as_ref()
                    .is_some_and(|threads| !threads.contains(&thread))
                {
                    continue;
                }
                let samples = capture.thread_samples(range);
                for sample in &samples[in_range(samples, |sample| sample.time.get(), query.range)] {
                    let (stack, task) = (sample.stack.get(), sample.task.get());
                    if stack != NONE && task_kept(task) {
                        take(Selected {
                            time: sample.time.get(),
                            thread,
                            task,
                            stack,
                            weight: sample.weight.get(),
                        });
                    }
                }
            }
        }
        Source::Waits => {
            let waits = &capture.task_waits;
            for wait in &waits[in_range(waits, |wait| wait.time.get(), query.range)] {
                let (stack, task) = (wait.stack.get(), wait.task.get());
                if stack != NONE && task_kept(task) {
                    take(Selected {
                        time: wait.time.get(),
                        thread: NONE,
                        task,
                        stack,
                        weight: wait.weight.get(),
                    });
                }
            }
        }
    }
}

/// The weight and count of what `query` selects on each stack and, by
/// task, of each task on it: (stack, task, weight, count), the task
/// [`NONE`] unless the query is by task.
fn select(capture: &Capture, query: &Query) -> Vec<(u32, u32, u64, u32)> {
    if query.by_task {
        let mut by: HashMap<(u32, u32), (u64, u32)> = HashMap::default();
        each(capture, query, |selected| {
            let entry = by.entry((selected.stack, selected.task)).or_default();
            entry.0 += selected.weight;
            entry.1 += 1;
        });
        let mut selected: Vec<(u32, u32, u64, u32)> = by
            .into_iter()
            .map(|((stack, task), (weight, count))| (stack, task, weight, count))
            .collect();
        selected.sort_unstable();
        return selected;
    }
    let mut weights = vec![0_u64; capture.stacks.len()];
    let mut counts = vec![0_u32; capture.stacks.len()];
    each(capture, query, |selected| {
        weights[selected.stack as usize] += selected.weight;
        counts[selected.stack as usize] += 1;
    });
    weights
        .into_iter()
        .zip(counts)
        .enumerate()
        .filter(|(_, (weight, _))| *weight > 0)
        .map(|(stack, (weight, count))| (index(stack), NONE, weight, count))
        .collect()
}

/// The positions of a time-ordered run's rows in `range`.
fn in_range<T>(
    rows: &[T],
    time: impl Fn(&T) -> i64,
    range: Option<(i64, i64)>,
) -> std::ops::Range<usize> {
    let Some((start, end)) = range else {
        return 0..rows.len();
    };
    let first = rows.partition_point(|row| time(row) < start);
    let last = rows.partition_point(|row| time(row) < end);
    first..last.max(first)
}

/// Turns stacks into transformed paths.
struct Paths<'a> {
    capture: &'a Capture,
    keys: &'a Keys<'a>,
    query: &'a Query,
    path: Vec<u32>,
}

impl<'a> Paths<'a> {
    const fn new(capture: &'a Capture, keys: &'a Keys<'a>, query: &'a Query) -> Self {
        Self {
            capture,
            keys,
            query,
            path: Vec::new(),
        }
    }

    /// The path of `stack`, run or waited on in `task`, under the query, or
    /// `None` when a transform removes it.
    fn of(&mut self, stack: u32, task: u32) -> Option<&[u32]> {
        let path = &mut self.path;
        path.clear();
        for frame in self.capture.stack_frames(stack) {
            path.extend_from_slice(self.keys.frame(frame));
        }
        if self.query.logical {
            logical(self.keys, path);
        }
        // Innermost first, as stacks are; a tree from the roots starts
        // with the outermost.
        if !self.query.invert {
            path.reverse();
        }
        for transform in &self.query.transforms {
            match *transform {
                Transform::Focus(key) => {
                    let first = path.iter().position(|candidate| *candidate == key)?;
                    path.drain(..first);
                }
                Transform::Merge(key) => path.retain(|candidate| *candidate != key),
                Transform::Drop(key) => {
                    if path.contains(&key) {
                        return None;
                    }
                }
                Transform::CollapseRecursion(key) => {
                    let mut previous = NONE;
                    path.retain(|candidate| {
                        let repeat = *candidate == key && previous == key;
                        previous = *candidate;
                        !repeat
                    });
                }
            }
        }
        if self.query.by_task {
            let group = self.keys.group(task);
            if self.query.invert {
                path.push(group);
            } else {
                path.insert(0, group);
            }
        }
        Some(path)
    }
}

/// Makes an innermost-first path logical. The runtime gives the thread to
/// a task at the innermost `Dispatch` frame, so the frames inside it are
/// the task's and the scheduler's outside it are dropped. The task's own
/// code begins at its outermost frame that is neither runtime machinery
/// nor a wrapper, which is rooted under a key for the task; the machinery
/// between the dispatch and it goes. Wrappers only forward, so they fold
/// away everywhere. A path with no dispatch, such as a goroutine's, which
/// runs on a stack of its own, keeps its frames.
fn logical(keys: &Keys<'_>, path: &mut Vec<u32>) {
    if let Some(dispatch) = path
        .iter()
        .position(|key| keys.role(*key) == CodeRole::Dispatch)
    {
        path.truncate(dispatch + 1);
        let machinery = |key: &u32| {
            matches!(
                keys.role(*key),
                CodeRole::RuntimeInternal | CodeRole::Wrapper
            )
        };
        if let Some(root) = path[..dispatch].iter().rposition(|key| !machinery(key))
            && let Some(task) = keys.task(path[root])
        {
            path.splice(root + 1..dispatch, [task]);
        }
    }
    path.retain(|key| keys.role(*key) != CodeRole::Wrapper);
}

/// The call tree of the samples `query` selects.
pub fn aggregate(capture: &Capture, keys: &Keys<'_>, query: &Query) -> CallTree {
    let selected = select(capture, query);
    let mut paths = Paths::new(capture, keys, query);
    // The tree in the order nodes are first reached.
    let mut key = Vec::new();
    let mut parent = Vec::new();
    let mut own = Vec::new();
    let mut children: HashMap<(u32, u32), u32> = HashMap::default();
    let mut tree = CallTree::default();
    for &(stack, task, weight, count) in &selected {
        let Some(path) = paths.of(stack, task) else {
            continue;
        };
        tree.weight += weight;
        tree.samples += u64::from(count);
        let mut node = NONE;
        for &step in path {
            node = *children.entry((node, step)).or_insert_with(|| {
                key.push(step);
                parent.push(node);
                own.push(0);
                index(key.len() - 1)
            });
        }
        if node != NONE {
            own[node as usize] += weight;
        }
    }
    // A node is reached after its parent, so a reverse pass totals it.
    let mut total = own.clone();
    for node in (0..key.len()).rev() {
        if parent[node] != NONE {
            total[parent[node] as usize] += total[node];
        }
    }
    // Lay the tree out depth first, largest child first.
    let mut kids: Vec<Vec<u32>> = vec![Vec::new(); key.len()];
    let mut roots = Vec::new();
    for (node, caller) in parent.iter().enumerate() {
        match *caller {
            NONE => roots.push(index(node)),
            caller => kids[caller as usize].push(index(node)),
        }
    }
    let largest_first = |nodes: &mut Vec<u32>| {
        nodes.sort_by(|a, b| {
            total[*b as usize]
                .cmp(&total[*a as usize])
                .then(key[*a as usize].cmp(&key[*b as usize]))
        });
    };
    largest_first(&mut roots);
    for list in &mut kids {
        largest_first(list);
    }
    let mut placed = vec![NONE; key.len()];
    let mut pending: Vec<(u32, u32)> = roots.iter().rev().map(|root| (*root, 0)).collect();
    while let Some((node, depth)) = pending.pop() {
        let position = index(tree.key.len());
        placed[node as usize] = position;
        tree.key.push(key[node as usize]);
        tree.parent.push(match parent[node as usize] {
            NONE => NONE,
            caller => placed[caller as usize],
        });
        tree.depth.push(depth);
        tree.own.push(own[node as usize]);
        tree.total.push(total[node as usize]);
        tree.end.push(position + 1);
        pending.extend(
            kids[node as usize]
                .iter()
                .rev()
                .map(|kid| (*kid, depth + 1)),
        );
    }
    // Each subtree ends where the next node of its depth or less begins.
    for position in (0..tree.len()).rev() {
        let caller = tree.parent[position];
        if caller != NONE {
            let end = tree.end[position];
            let caller_end = &mut tree.end[caller as usize];
            *caller_end = (*caller_end).max(end);
        }
    }
    tree
}

/// What each key costs under a query, most own weight first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Costs {
    pub rows: Vec<Cost>,
    /// The weight of every sample selected and kept.
    pub weight: u64,
}

/// What each key costs under `query`.
pub fn functions(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Costs {
    let selected = select(capture, query);
    let mut paths = Paths::new(capture, keys, query);
    let mut own = vec![0_u64; keys.count()];
    let mut total = vec![0_u64; keys.count()];
    // The entry that last counted a key, so recursion counts once.
    let mut counted = vec![u32::MAX; keys.count()];
    let mut kept = 0;
    for (entry, &(stack, task, weight, _)) in selected.iter().enumerate() {
        let Some(path) = paths.of(stack, task) else {
            continue;
        };
        kept += weight;
        let innermost = if query.invert {
            path.first()
        } else {
            path.last()
        };
        if let Some(&innermost) = innermost {
            own[innermost as usize] += weight;
        }
        for &key in path {
            if counted[key as usize] != index(entry) {
                counted[key as usize] = index(entry);
                total[key as usize] += weight;
            }
        }
    }
    let mut costs: Vec<Cost> = (0..keys.count())
        .filter(|&key| total[key] > 0)
        .map(|key| Cost {
            key: index(key),
            own: own[key],
            total: total[key],
        })
        .collect();
    costs.sort_by(|a, b| (b.own, b.total, a.key).cmp(&(a.own, a.total, b.key)));
    Costs {
        rows: costs,
        weight: kept,
    }
}

/// One thread's nanoseconds off its CPU in each slice of a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffCpuLane {
    pub thread: u32,
    pub blocked: Vec<u64>,
    pub preempted: Vec<u64>,
}

/// How long each selected thread was off its CPU, blocked and preempted,
/// in each of `buckets` equal slices of the query's range, per thread
/// with any. Intervals have no stacks, so transforms do not apply.
pub fn off_cpu(capture: &Capture, query: &Query, buckets: usize) -> Vec<OffCpuLane> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };
    let span = u128::try_from(end - start).unwrap_or(1);
    let count = buckets as u128;
    // Slice `i` is [edge(i), edge(i + 1)).
    let edge = |slice: usize| {
        start + i64::try_from(slice as u128 * span / count).unwrap_or(i64::MAX - start)
    };
    let mut lanes = Vec::new();
    for range in &capture.off_cpu_ranges {
        let thread = range.thread.get();
        if query
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&thread))
        {
            continue;
        }
        let mut lane = OffCpuLane {
            thread,
            blocked: vec![0; buckets],
            preempted: vec![0; buckets],
        };
        let mut any = false;
        for interval in capture.thread_off_cpu(range) {
            let left = interval.time.get();
            let back = left.saturating_add_unsigned(interval.duration.get());
            let (from, to) = (left.max(start), back.min(end));
            if from >= to {
                continue;
            }
            let totals = if interval.preempted == 0 {
                &mut lane.blocked
            } else {
                &mut lane.preempted
            };
            let offset = u128::try_from(from - start).unwrap_or(0);
            let mut slice = usize::try_from(offset * count / span)
                .unwrap_or(0)
                .min(buckets - 1);
            while slice < buckets && edge(slice) < to {
                let overlap = to.min(edge(slice + 1)) - from.max(edge(slice));
                totals[slice] += u64::try_from(overlap).unwrap_or(0);
                slice += 1;
            }
            any = true;
        }
        if any {
            lanes.push(lane);
        }
    }
    lanes
}

/// What a timeline's lanes are of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lanes {
    /// A lane per thread, by its row.
    #[default]
    Threads,
    /// A lane per task, by its row, and one, [`NONE`], for what no task
    /// snapshot named.
    Tasks,
}

/// The weight of what `query` keeps in each of `buckets` equal slices of
/// its range, per lane with any. Waits are of no thread, so their lanes
/// are always by task.
pub fn density(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &Query,
    buckets: usize,
    lanes: Lanes,
) -> Vec<(u32, Vec<u64>)> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };
    let by_task = lanes == Lanes::Tasks || query.source == Source::Waits;
    let span = u128::try_from(end - start).unwrap_or(1);
    let mut paths = Paths::new(capture, keys, query);
    // Whether each stack's samples are kept, decided once per stack and
    // task, and each lane, the last for no task.
    let mut kept: HashMap<(u32, u32), bool> = HashMap::default();
    let count = if by_task {
        capture.tasks.len() + 1
    } else {
        capture.threads.len()
    };
    let mut slices: Vec<Option<Vec<u64>>> = vec![None; count];
    each(capture, query, |selected| {
        let task = if query.by_task { selected.task } else { NONE };
        let keep = *kept
            .entry((selected.stack, task))
            .or_insert_with(|| paths.of(selected.stack, task).is_some());
        if !keep {
            return;
        }
        let lane = match (by_task, selected.task) {
            (true, NONE) => capture.tasks.len(),
            (true, task) => task as usize,
            (false, _) => selected.thread as usize,
        };
        let offset = u128::try_from(selected.time - start).unwrap_or(0);
        let bucket = usize::try_from(offset * buckets as u128 / span).unwrap_or(0);
        slices[lane].get_or_insert_with(|| vec![0; buckets])[bucket.min(buckets - 1)] +=
            selected.weight;
    });
    slices
        .into_iter()
        .enumerate()
        .filter_map(|(lane, slices)| {
            let lane = match (by_task, lane == capture.tasks.len()) {
                (true, true) => NONE,
                _ => index(lane),
            };
            Some((lane, slices?))
        })
        .collect()
}
