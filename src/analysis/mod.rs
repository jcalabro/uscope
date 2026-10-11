//! Queries of a capture, pure functions of its tables: which functions
//! cost the most, and the call tree samples make.
//!
//! A frame stands for each function its code runs: its source lines, one
//! per inlined function and then the one it is compiled into. A frame
//! without symbols stands for itself, and a truncation for its reason.
//! Every function, symbol-less frame, and truncation is a dense key.

use foldhash::HashMap;

use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, Truncation};

/// Each frame's keys, innermost first.
pub struct Keys<'a> {
    capture: &'a Capture,
    starts: Vec<u32>,
    flat: Vec<u32>,
}

impl<'a> Keys<'a> {
    pub fn new(capture: &'a Capture) -> Self {
        let functions = index(capture.functions.len());
        let frames = index(capture.frames.len());
        let mut starts = Vec::with_capacity(capture.frames.len() + 1);
        let mut flat = Vec::with_capacity(capture.frames.len());
        for (frame, record) in capture.frames.iter().enumerate() {
            starts.push(index(flat.len()));
            let frame = index(frame);
            if record.kind == FrameKind::Truncated as u8 {
                let reason =
                    Truncation::from_number(record.address.get()).map_or(0, |reason| reason as u32);
                flat.push(functions + frames + reason);
                continue;
            }
            let lines = capture.frame_lines(frame);
            if lines.is_empty() {
                flat.push(functions + frame);
            }
            flat.extend(lines.iter().map(|line| line.function.get()));
        }
        starts.push(index(flat.len()));
        Self {
            capture,
            starts,
            flat,
        }
    }

    /// How many keys there can be.
    pub const fn count(&self) -> usize {
        self.capture.functions.len() + self.capture.frames.len() + Truncation::ALL.len() + 1
    }

    /// A frame's keys, innermost first.
    pub fn frame(&self, frame: u32) -> &[u32] {
        let frame = frame as usize;
        &self.flat[self.starts[frame] as usize..self.starts[frame + 1] as usize]
    }

    /// How a view names a key.
    pub fn name(&self, key: u32) -> String {
        let capture = self.capture;
        let functions = index(capture.functions.len());
        let frames = index(capture.frames.len());
        if key < functions {
            return capture
                .string(capture.functions[key as usize].name.get())
                .to_owned();
        }
        if key < functions + frames {
            let record = &capture.frames[(key - functions) as usize];
            let module = record.module.get();
            if module == NONE {
                return format!("{:#x}", record.address.get());
            }
            let path = capture.string(capture.modules[module as usize].path.get());
            let name = path.rsplit('/').next().unwrap_or(path);
            return format!("{name}+{:#x}", record.address.get());
        }
        let reason = Truncation::from_number(u64::from(key - functions - frames))
            .map_or("unknown", Truncation::describe);
        format!("[{reason}]")
    }
}

/// What one key costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub key: u32,
    /// Weight of samples it was the innermost function of.
    pub own: u64,
    /// Weight of samples it was anywhere on the stack of.
    pub total: u64,
}

/// Every key's cost, most expensive first.
pub fn top(capture: &Capture, keys: &Keys<'_>) -> Vec<Cost> {
    let count = keys.count();
    let mut own = vec![0_u64; count];
    let mut total = vec![0_u64; count];
    // The sample that last counted a key, so recursion counts once.
    let mut counted = vec![usize::MAX; count];
    for (index, sample) in capture.samples.iter().enumerate() {
        let weight = sample.weight.get();
        let mut innermost = true;
        for frame in capture.stack_frames(sample.stack.get()) {
            for &key in keys.frame(frame) {
                let key = key as usize;
                if innermost {
                    own[key] += weight;
                    innermost = false;
                }
                if counted[key] != index {
                    counted[key] = index;
                    total[key] += weight;
                }
            }
        }
    }
    let mut costs: Vec<Cost> = (0..count)
        .filter(|&key| total[key] > 0)
        .map(|key| Cost {
            key: index(key),
            own: own[key],
            total: total[key],
        })
        .collect();
    costs.sort_by(|a, b| (b.own, b.total, a.key).cmp(&(a.own, a.total, b.key)));
    costs
}

/// A node of the call tree: a key called from its parent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// The caller's node, or [`NONE`] for a root.
    pub parent: u32,
    pub key: u32,
    pub own: u64,
    pub total: u64,
}

/// The call tree, from outermost callers in. Parents come before their
/// children.
pub fn tree(capture: &Capture, keys: &Keys<'_>) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut children: HashMap<(u32, u32), u32> = HashMap::default();
    // Stacks are a tree already, of frames from the outermost in: each
    // stack's node is its frame's innermost key under its parent's.
    let mut stack_nodes = Vec::with_capacity(capture.stacks.len());
    for stack in &capture.stacks {
        let mut node = match stack.parent.get() {
            NONE => NONE,
            parent => stack_nodes[parent as usize],
        };
        for &key in keys.frame(stack.frame.get()).iter().rev() {
            node = *children.entry((node, key)).or_insert_with(|| {
                nodes.push(Node {
                    parent: node,
                    key,
                    own: 0,
                    total: 0,
                });
                index(nodes.len() - 1)
            });
        }
        stack_nodes.push(node);
    }
    for sample in &capture.samples {
        let node = stack_nodes[sample.stack.get() as usize];
        if node != NONE {
            nodes[node as usize].own += sample.weight.get();
        }
    }
    for index in (0..nodes.len()).rev() {
        let node = nodes[index];
        let total = node.total + node.own;
        nodes[index].total = total;
        if node.parent != NONE {
            nodes[node.parent as usize].total += total;
        }
    }
    nodes
}

/// A position in a capture's tables, which validation keeps within `u32`.
fn index(position: usize) -> u32 {
    u32::try_from(position).expect("a capture's tables are indexed by u32")
}
