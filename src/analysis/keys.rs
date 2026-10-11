//! What each frame stands for, as dense keys.

use foldhash::HashMap;

use super::{CallTree, Costs, Query, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, Truncation, role_of};

/// How finely frames are told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Granularity {
    /// By function, each inlined function a frame of its own.
    #[default]
    Function,
    /// By function, without the functions inlined into it.
    PhysicalFunction,
    /// By source line of each function.
    Line,
    /// By instruction.
    Address,
}

/// What a key stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// A function, by its row in the capture's functions.
    Function { function: u32 },
    /// A line of a function: its file, or [`NONE`], and number.
    Line { function: u32, file: u32, line: u32 },
    /// A frame no symbols describe, or any frame at address granularity.
    Address { frame: u32 },
    /// Where a stack ended early, and the module at fault, or [`NONE`].
    Truncation { reason: u8, module: u32 },
    /// The root of an async task's logical stack, by the outermost code of
    /// its own: a function, or a frame no symbols describe, the other
    /// [`NONE`].
    Task { function: u32, frame: u32 },
    /// A task of a language runtime, by its row, under which a query by
    /// task roots each path: [`NONE`] for what no task snapshot named.
    Group { task: u32 },
}

/// Each frame's keys, innermost first, and what each key is.
pub struct Keys<'a> {
    capture: &'a Capture,
    granularity: Granularity,
    starts: Vec<u32>,
    flat: Vec<u32>,
    kinds: Vec<KeyKind>,
    roles: Vec<CodeRole>,
    /// Each key's task root, or [`NONE`] for a key that roots none.
    tasks: Vec<u32>,
    /// The group key of each task, by its row, and last, of none.
    groups: Vec<u32>,
}

impl<'a> Keys<'a> {
    pub(crate) fn new(capture: &'a Capture, granularity: Granularity) -> Self {
        let mut kinds = Vec::new();
        let mut known: HashMap<KeyKind, u32> = HashMap::default();
        let mut key = |kind: KeyKind| {
            *known.entry(kind).or_insert_with(|| {
                kinds.push(kind);
                index(kinds.len() - 1)
            })
        };
        let mut starts = Vec::with_capacity(capture.frames.len() + 1);
        let mut flat = Vec::with_capacity(capture.frames.len());
        for (frame, record) in capture.frames.iter().enumerate() {
            starts.push(index(flat.len()));
            let frame = index(frame);
            if record.kind == FrameKind::Truncated as u8 {
                let reason =
                    Truncation::from_number(record.address.get()).map_or(0, |reason| reason as u8);
                flat.push(key(KeyKind::Truncation {
                    reason,
                    module: record.module.get(),
                }));
                continue;
            }
            let lines = capture.frame_lines(frame);
            let lines = match granularity {
                Granularity::Address => &[][..],
                // The function the code is compiled into is the last line.
                Granularity::PhysicalFunction => lines.last().map_or(&[][..], std::slice::from_ref),
                Granularity::Function | Granularity::Line => lines,
            };
            if lines.is_empty() {
                flat.push(key(KeyKind::Address { frame }));
            }
            for line in lines {
                let function = line.function.get();
                flat.push(key(if granularity == Granularity::Line {
                    KeyKind::Line {
                        function,
                        file: line.file.get(),
                        line: line.number.get(),
                    }
                } else {
                    KeyKind::Function { function }
                }));
            }
        }
        starts.push(index(flat.len()));
        let mut roles: Vec<CodeRole> = kinds
            .iter()
            .map(|kind| {
                let number = match *kind {
                    KeyKind::Function { function } | KeyKind::Line { function, .. } => {
                        capture.functions[function as usize].role
                    }
                    KeyKind::Address { frame } => capture.frames[frame as usize].role,
                    KeyKind::Truncation { .. } | KeyKind::Task { .. } | KeyKind::Group { .. } => 0,
                };
                role_of(number).unwrap_or_default()
            })
            .collect();
        // A task is rooted by function, whatever the granularity.
        let mut tasks = vec![NONE; kinds.len()];
        for (code, kind) in kinds.clone().into_iter().enumerate() {
            let task = match kind {
                KeyKind::Function { function } | KeyKind::Line { function, .. } => KeyKind::Task {
                    function,
                    frame: NONE,
                },
                KeyKind::Address { frame } => KeyKind::Task {
                    function: NONE,
                    frame,
                },
                KeyKind::Truncation { .. } | KeyKind::Task { .. } | KeyKind::Group { .. } => {
                    continue;
                }
            };
            tasks[code] = *known.entry(task).or_insert_with(|| {
                kinds.push(task);
                roles.push(CodeRole::Ordinary);
                index(kinds.len() - 1)
            });
        }
        let groups = (0..index(capture.tasks.len()))
            .chain([NONE])
            .map(|task| {
                kinds.push(KeyKind::Group { task });
                roles.push(CodeRole::Ordinary);
                index(kinds.len() - 1)
            })
            .collect();
        tasks.resize(kinds.len(), NONE);
        Self {
            capture,
            granularity,
            starts,
            flat,
            kinds,
            roles,
            tasks,
            groups,
        }
    }

    #[must_use]
    pub const fn granularity(&self) -> Granularity {
        self.granularity
    }

    /// How many keys there are.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.kinds.len()
    }

    /// A frame's keys, innermost first.
    #[must_use]
    pub fn frame(&self, frame: u32) -> &[u32] {
        let frame = frame as usize;
        &self.flat[self.starts[frame] as usize..self.starts[frame + 1] as usize]
    }

    #[must_use]
    pub fn kind(&self, key: u32) -> KeyKind {
        self.kinds[key as usize]
    }

    /// The role of a key's code.
    #[must_use]
    pub fn role(&self, key: u32) -> CodeRole {
        self.roles[key as usize]
    }

    /// The root of a task whose outermost code of its own is `key`'s.
    #[must_use]
    pub fn task(&self, key: u32) -> Option<u32> {
        Some(self.tasks[key as usize]).filter(|task| *task != NONE)
    }

    /// The key a query by task roots `task`'s paths at, by its row, or
    /// [`NONE`] for what no task snapshot named.
    #[must_use]
    pub fn group(&self, task: u32) -> u32 {
        self.groups
            .get(task as usize)
            .copied()
            .unwrap_or(self.groups[self.groups.len() - 1])
    }

    /// The key of `kind`, if any frame has it.
    pub fn find(&self, kind: KeyKind) -> Option<u32> {
        self.kinds
            .iter()
            .position(|known| *known == kind)
            .map(index)
    }

    /// Every key a view names `name`.
    #[must_use]
    pub fn named(&self, name: &str) -> Vec<u32> {
        (0..index(self.count()))
            .filter(|key| self.name(*key) == name)
            .collect()
    }

    /// The call tree of the samples `query` selects.
    #[must_use]
    pub fn aggregate(&self, query: &Query) -> CallTree {
        super::aggregate(self.capture, self, query)
    }

    /// What each key costs under `query`, most own weight first.
    #[must_use]
    pub fn functions(&self, query: &Query) -> Costs {
        super::functions(self.capture, self, query)
    }

    /// The weight of what `query` keeps in each of `buckets` slices of its
    /// range, per lane with any.
    #[must_use]
    pub fn density(
        &self,
        query: &Query,
        buckets: usize,
        lanes: super::Lanes,
    ) -> Vec<(u32, Vec<u64>)> {
        super::density(self.capture, self, query, buckets, lanes)
    }

    /// How a view names a key: a function's name, an address by module and
    /// offset, or a truncation's reason.
    pub fn name(&self, key: u32) -> String {
        let capture = self.capture;
        match self.kind(key) {
            KeyKind::Function { function } => capture
                .string(capture.functions[function as usize].name.get())
                .to_owned(),
            KeyKind::Line {
                function,
                file,
                line,
            } => {
                let name = capture.string(capture.functions[function as usize].name.get());
                match file {
                    NONE => format!("{name}:{line}"),
                    file => {
                        let path = capture.string(file);
                        let file = path.rsplit('/').next().unwrap_or(path);
                        format!("{name} {file}:{line}")
                    }
                }
            }
            KeyKind::Address { frame } => self.address_name(frame),
            KeyKind::Task { function, frame } => {
                let root = match function {
                    NONE => self.address_name(frame),
                    function => capture
                        .string(capture.functions[function as usize].name.get())
                        .to_owned(),
                };
                format!("[task] {root}")
            }
            KeyKind::Group { task: NONE } => "[no task known]".to_owned(),
            KeyKind::Group { task } => {
                let task = &capture.tasks[task as usize];
                let name = format!(
                    "{} {}",
                    capture.string(task.runtime.get()),
                    task.number.get()
                );
                match task.labels.get() {
                    NONE => name,
                    labels => {
                        let labels: Vec<&str> = capture.string(labels).lines().collect();
                        format!("{name} {{{}}}", labels.join(", "))
                    }
                }
            }
            KeyKind::Truncation { reason, module } => {
                let reason = Truncation::from_number(u64::from(reason))
                    .map_or("unknown", Truncation::describe);
                self.module_name(module).map_or_else(
                    || format!("[{reason}]"),
                    |module| format!("[{reason}: {module}]"),
                )
            }
        }
    }

    /// The source file and line a key names, if any: a line's own, or where
    /// a function is declared.
    #[must_use]
    pub fn location(&self, key: u32) -> Option<(&'a str, u32)> {
        let capture = self.capture;
        let (file, line) = match self.kind(key) {
            KeyKind::Line { file, line, .. } => (file, line),
            KeyKind::Function { function } => {
                let function = &capture.functions[function as usize];
                (function.file.get(), function.line.get())
            }
            KeyKind::Task { function, .. } if function != NONE => {
                let function = &capture.functions[function as usize];
                (function.file.get(), function.line.get())
            }
            KeyKind::Address { .. }
            | KeyKind::Truncation { .. }
            | KeyKind::Task { .. }
            | KeyKind::Group { .. } => {
                return None;
            }
        };
        (file != NONE).then(|| (capture.string(file), line))
    }

    /// The module a key's code belongs to, by its file name.
    #[must_use]
    pub fn module(&self, key: u32) -> Option<&'a str> {
        let capture = self.capture;
        let module = match self.kind(key) {
            KeyKind::Function { function }
            | KeyKind::Line { function, .. }
            | KeyKind::Task {
                function,
                frame: NONE,
            } => capture.functions[function as usize].module.get(),
            KeyKind::Address { frame } | KeyKind::Task { frame, .. } => {
                capture.frames[frame as usize].module.get()
            }
            KeyKind::Truncation { module, .. } => module,
            KeyKind::Group { .. } => NONE,
        };
        self.module_name(module)
    }

    fn address_name(&self, frame: u32) -> String {
        let record = &self.capture.frames[frame as usize];
        let address = record.address.get();
        self.module_name(record.module.get()).map_or_else(
            || format!("{address:#x}"),
            |module| format!("{module}+{address:#x}"),
        )
    }

    fn module_name(&self, module: u32) -> Option<&'a str> {
        let capture = self.capture;
        (module != NONE).then(|| {
            let path = capture.string(capture.modules[module as usize].path.get());
            path.rsplit('/').next().unwrap_or(path)
        })
    }
}
