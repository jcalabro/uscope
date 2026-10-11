//! What each frame stands for, as dense keys.

use foldhash::HashMap;

use super::index;
use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, Truncation};

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
}

/// Each frame's keys, innermost first, and what each key is.
pub struct Keys<'a> {
    capture: &'a Capture,
    granularity: Granularity,
    starts: Vec<u32>,
    flat: Vec<u32>,
    kinds: Vec<KeyKind>,
}

impl<'a> Keys<'a> {
    pub fn new(capture: &'a Capture, granularity: Granularity) -> Self {
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
        Self {
            capture,
            granularity,
            starts,
            flat,
            kinds,
        }
    }

    pub const fn granularity(&self) -> Granularity {
        self.granularity
    }

    /// How many keys there are.
    pub const fn count(&self) -> usize {
        self.kinds.len()
    }

    /// A frame's keys, innermost first.
    pub fn frame(&self, frame: u32) -> &[u32] {
        let frame = frame as usize;
        &self.flat[self.starts[frame] as usize..self.starts[frame + 1] as usize]
    }

    pub fn kind(&self, key: u32) -> KeyKind {
        self.kinds[key as usize]
    }

    /// The key of `kind`, if any frame has it.
    pub fn find(&self, kind: KeyKind) -> Option<u32> {
        self.kinds
            .iter()
            .position(|known| *known == kind)
            .map(index)
    }

    /// How a view names a key: a function's name, an address by module and
    /// offset, or a truncation's reason.
    pub fn name(&self, key: u32) -> String {
        let capture = self.capture;
        match self.kind(key) {
            KeyKind::Function { function } | KeyKind::Line { function, .. } => capture
                .string(capture.functions[function as usize].name.get())
                .to_owned(),
            KeyKind::Address { frame } => {
                let record = &capture.frames[frame as usize];
                let address = record.address.get();
                self.module_name(record.module.get()).map_or_else(
                    || format!("{address:#x}"),
                    |module| format!("{module}+{address:#x}"),
                )
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
    pub fn location(&self, key: u32) -> Option<(&'a str, u32)> {
        let capture = self.capture;
        let (file, line) = match self.kind(key) {
            KeyKind::Line { file, line, .. } => (file, line),
            KeyKind::Function { function } => {
                let function = &capture.functions[function as usize];
                (function.file.get(), function.line.get())
            }
            KeyKind::Address { .. } | KeyKind::Truncation { .. } => return None,
        };
        (file != NONE).then(|| (capture.string(file), line))
    }

    /// The module a key's code belongs to, by its file name.
    pub fn module(&self, key: u32) -> Option<&'a str> {
        let capture = self.capture;
        let module = match self.kind(key) {
            KeyKind::Function { function } | KeyKind::Line { function, .. } => {
                capture.functions[function as usize].module.get()
            }
            KeyKind::Address { frame } => capture.frames[frame as usize].module.get(),
            KeyKind::Truncation { module, .. } => module,
        };
        self.module_name(module)
    }

    fn module_name(&self, module: u32) -> Option<&'a str> {
        let capture = self.capture;
        (module != NONE).then(|| {
            let path = capture.string(capture.modules[module as usize].path.get());
            path.rsplit('/').next().unwrap_or(path)
        })
    }
}
