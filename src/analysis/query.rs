//! Aggregating a selection of samples into a call tree, function costs,
//! and densities over time.
//!
//! A query selects samples by time and thread, turns each sample's stack
//! into a path of keys, outermost first or, inverted, innermost first,
//! and reshapes the path by its transforms in order. A logical query
//! first shows each async task's frames as a stack of their own. Samples are counted
//! per distinct stack first, so the work is linear in the samples selected
//! and the stacks they use.

use foldhash::HashMap;

use super::{Keys, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::{NONE, Sample};

/// Which samples, seen how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Samples taken in `[start, end)`, or all.
    pub range: Option<(i64, i64)>,
    /// Samples of these threads, by their rows in the capture, or of all.
    pub threads: Option<Vec<u32>>,
    pub transforms: Vec<Transform>,
    /// Paths from the innermost frame out, so that the tree is of callers.
    pub invert: bool,
    /// Each async task's frames as a stack of their own (see [`logical`]),
    /// before any transform.
    pub logical: bool,
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

/// The selected samples' weight and count on each stack, by stack row.
fn select(capture: &Capture, query: &Query) -> (Vec<u64>, Vec<u32>) {
    let mut weights = vec![0_u64; capture.stacks.len()];
    let mut counts = vec![0_u32; capture.stacks.len()];
    for range in &capture.sample_ranges {
        if query
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&range.thread.get()))
        {
            continue;
        }
        for sample in in_range(capture.thread_samples(range), query.range) {
            let stack = sample.stack.get();
            if stack == NONE {
                continue;
            }
            weights[stack as usize] += sample.weight.get();
            counts[stack as usize] += 1;
        }
    }
    (weights, counts)
}

/// The samples of a thread's time-ordered run in `range`.
fn in_range(samples: &[Sample], range: Option<(i64, i64)>) -> &[Sample] {
    let Some((start, end)) = range else {
        return samples;
    };
    let first = samples.partition_point(|sample| sample.time.get() < start);
    let last = samples.partition_point(|sample| sample.time.get() < end);
    &samples[first..last.max(first)]
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

    /// The path of `stack` under the query, or `None` when a transform
    /// removes the sample.
    fn of(&mut self, stack: u32) -> Option<&[u32]> {
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
    let (weights, counts) = select(capture, query);
    let mut paths = Paths::new(capture, keys, query);
    // The tree in the order nodes are first reached.
    let mut key = Vec::new();
    let mut parent = Vec::new();
    let mut own = Vec::new();
    let mut children: HashMap<(u32, u32), u32> = HashMap::default();
    let mut tree = CallTree::default();
    for (stack, &weight) in weights.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let Some(path) = paths.of(index(stack)) else {
            continue;
        };
        tree.weight += weight;
        tree.samples += u64::from(counts[stack]);
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
    let (weights, _) = select(capture, query);
    let mut paths = Paths::new(capture, keys, query);
    let mut own = vec![0_u64; keys.count()];
    let mut total = vec![0_u64; keys.count()];
    // The stack that last counted a key, so recursion counts once.
    let mut counted = vec![u32::MAX; keys.count()];
    let mut kept = 0;
    for (stack, &weight) in weights.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let Some(path) = paths.of(index(stack)) else {
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
            if counted[key as usize] != index(stack) {
                counted[key as usize] = index(stack);
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

/// How many kept samples, and their weight, fall in each of `buckets`
/// equal slices of the query's range, per selected thread with any.
pub fn density(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &Query,
    buckets: usize,
) -> Vec<(u32, Vec<u64>)> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };
    let span = u128::try_from(end - start).unwrap_or(1);
    let mut paths = Paths::new(capture, keys, query);
    // Whether each stack's samples are kept, decided once per stack.
    let mut kept: Vec<Option<bool>> = vec![None; capture.stacks.len()];
    let mut lanes = Vec::new();
    for range in &capture.sample_ranges {
        let thread = range.thread.get();
        if query
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&thread))
        {
            continue;
        }
        let mut lane = vec![0_u64; buckets];
        let mut any = false;
        for sample in in_range(capture.thread_samples(range), query.range) {
            let stack = sample.stack.get();
            if stack == NONE {
                continue;
            }
            let keep = *kept[stack as usize].get_or_insert_with(|| paths.of(stack).is_some());
            if !keep {
                continue;
            }
            let offset = u128::try_from(sample.time.get() - start).unwrap_or(0);
            let bucket = usize::try_from(offset * buckets as u128 / span).unwrap_or(0);
            lane[bucket.min(buckets - 1)] += sample.weight.get();
            any = true;
        }
        if any {
            lanes.push((thread, lane));
        }
    }
    lanes
}
