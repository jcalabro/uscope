//! Profiles as data: what a recording saw, in interned tables.
//!
//! A [`Capture`] is a set of tables of fixed-size records that refer to
//! each other by index: strings, modules, processes and threads, the
//! mappings that placed modules in processes, distinct frames, a stack
//! tree whose parents precede their children, and samples grouped by
//! thread in time order. Instrumented zones, task waits, and the counts of
//! everything lost or truncated live beside them. The rows in memory are
//! the rows a capture file holds: a capture seals into one image of the
//! shared container (see [`crate::image::container`]), validated before
//! any view reads it, and a file is a sequence of such chunks.
//!
//! The module is pure: it reads no process, file, or clock.

pub mod builder;
pub mod file;
pub mod records;
#[cfg(test)]
mod tests;

use zerocopy::{FromBytes as _, IntoBytes as _};

use crate::TargetDescription;
use crate::image::container::{self, Format, TableBytes, Tables};
use crate::image::{AlignedBytes, ImageError, Limits, StrId, Strings};

pub use builder::CaptureBuilder;
pub use records::*;

/// The capture format of the shared image container.
#[derive(Debug)]
pub struct CaptureFormat;

/// What a capture's tables mean: bump it with any change to what is
/// stored.
pub const CAPTURE_REVISION: u32 = 1;

impl Format for CaptureFormat {
    type Kind = CaptureTable;
    const MAGIC: [u8; 8] = *b"USCOPECP";
    const REVISION: u32 = CAPTURE_REVISION;
    const KINDS: usize = CaptureTable::COUNT;

    fn fingerprint() -> u64 {
        static FINGERPRINT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *FINGERPRINT
            .get_or_init(|| twox_hash::XxHash3_64::oneshot(records::schema_text().as_bytes()))
    }

    fn kind(number: u32) -> Option<CaptureTable> {
        CaptureTable::from_number(number)
    }

    fn number(kind: CaptureTable) -> u32 {
        kind as u32
    }

    fn index(kind: CaptureTable) -> usize {
        kind.index()
    }

    fn stride(kind: CaptureTable) -> usize {
        records::stride(kind)
    }
}

/// Every table of a capture, as rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    /// The string pool: NUL-terminated UTF-8, named by offset.
    pub strings: Vec<u8>,
    pub settings: Vec<Setting>,
    pub modules: Vec<Module>,
    pub processes: Vec<Process>,
    pub threads: Vec<Thread>,
    pub mappings: Vec<Mapping>,
    pub frames: Vec<Frame>,
    pub stacks: Vec<Stack>,
    pub samples: Vec<Sample>,
    pub sample_ranges: Vec<ThreadRange>,
    pub frame_symbols: Vec<Range>,
    pub lines: Vec<Line>,
    pub functions: Vec<Function>,
    pub accounting: Vec<Account>,
    pub off_cpu: Vec<Sample>,
    pub off_cpu_ranges: Vec<ThreadRange>,
    pub sites: Vec<Site>,
    pub zones: Vec<Zone>,
    pub zone_ranges: Vec<ZoneRange>,
    pub plots: Vec<Plot>,
    pub markers: Vec<Marker>,
    pub tasks: Vec<Task>,
    pub task_waits: Vec<TaskWait>,
}

/// The limits a capture is read within.
pub const LIMITS: Limits = Limits { bytes: 16 << 30 };

impl Capture {
    /// A string by its offset; empty for [`NONE`].
    pub fn string(&self, id: u32) -> &str {
        if id == NONE {
            return "";
        }
        Strings(&self.strings).get(StrId(id))
    }

    /// The value of a setting, if the capture records it.
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings
            .iter()
            .find(|setting| self.string(setting.key.get()) == key)
            .map(|setting| self.string(setting.value.get()))
    }

    /// A count from the accounting table; zero when absent.
    pub fn account(&self, kind: u32) -> u64 {
        self.accounting
            .iter()
            .filter(|account| account.kind.get() == kind)
            .map(|account| account.value.get())
            .sum()
    }

    /// One thread's on-CPU samples, in time order.
    pub fn thread_samples(&self, range: &ThreadRange) -> &[Sample] {
        rows(&self.samples, range.start.get(), range.count.get())
    }

    /// One thread's off-CPU intervals, in time order.
    pub fn thread_off_cpu(&self, range: &ThreadRange) -> &[Sample] {
        rows(&self.off_cpu, range.start.get(), range.count.get())
    }

    /// One frame's lines, innermost inline first; empty when the frame is
    /// not symbolized.
    pub fn frame_lines(&self, frame: u32) -> &[Line] {
        self.frame_symbols.get(frame as usize).map_or(&[], |range| {
            rows(&self.lines, range.start.get(), range.count.get())
        })
    }

    /// Frames from a stack node to its root, innermost first.
    pub fn stack_frames(&self, mut stack: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::from_fn(move || {
            let node = self.stacks.get(stack as usize)?;
            stack = node.parent.get();
            Some(node.frame.get())
        })
    }

    /// The capture's time span, from its settings.
    pub fn span(&self) -> (i64, i64) {
        let time = |key| {
            self.setting(key)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };
        (time("start"), time("end"))
    }

    /// Seals the capture into one chunk.
    pub fn seal(&self) -> Result<AlignedBytes, ImageError> {
        let mut tables = Tables::<CaptureFormat>::new();
        tables.add(
            CaptureTable::Strings,
            1,
            self.strings.len(),
            TableBytes::Borrowed(&self.strings),
        );
        macro_rules! put {
            ($field:ident, $table:ident) => {
                tables.add(
                    CaptureTable::$table,
                    records::stride(CaptureTable::$table),
                    self.$field.len(),
                    TableBytes::Borrowed(self.$field.as_bytes()),
                )
            };
        }
        put!(settings, Settings);
        put!(modules, Modules);
        put!(processes, Processes);
        put!(threads, Threads);
        put!(mappings, Mappings);
        put!(frames, Frames);
        put!(stacks, Stacks);
        put!(samples, Samples);
        put!(sample_ranges, SampleRanges);
        put!(frame_symbols, FrameSymbols);
        put!(lines, Lines);
        put!(functions, Functions);
        put!(accounting, Accounting);
        put!(off_cpu, OffCpu);
        put!(off_cpu_ranges, OffCpuRanges);
        put!(sites, Sites);
        put!(zones, Zones);
        put!(zone_ranges, ZoneRanges);
        put!(plots, Plots);
        put!(markers, Markers);
        put!(tasks, Tasks);
        put!(task_waits, TaskWaits);
        tables.seal(TargetDescription::X86_64)
    }

    /// Reads one chunk, validating it completely.
    pub fn from_chunk(bytes: &[u8]) -> Result<Self, ImageError> {
        let placed = container::structure::<CaptureFormat, { CaptureTable::COUNT }>(bytes, LIMITS)?;
        let table = |kind: CaptureTable| {
            placed[kind.index()].map_or(&[][..], |placed| {
                &bytes[placed.offset..placed.offset + placed.length]
            })
        };
        let capture = Self {
            strings: table(CaptureTable::Strings).to_vec(),
            settings: rows_of(table(CaptureTable::Settings)),
            modules: rows_of(table(CaptureTable::Modules)),
            processes: rows_of(table(CaptureTable::Processes)),
            threads: rows_of(table(CaptureTable::Threads)),
            mappings: rows_of(table(CaptureTable::Mappings)),
            frames: rows_of(table(CaptureTable::Frames)),
            stacks: rows_of(table(CaptureTable::Stacks)),
            samples: rows_of(table(CaptureTable::Samples)),
            sample_ranges: rows_of(table(CaptureTable::SampleRanges)),
            frame_symbols: rows_of(table(CaptureTable::FrameSymbols)),
            lines: rows_of(table(CaptureTable::Lines)),
            functions: rows_of(table(CaptureTable::Functions)),
            accounting: rows_of(table(CaptureTable::Accounting)),
            off_cpu: rows_of(table(CaptureTable::OffCpu)),
            off_cpu_ranges: rows_of(table(CaptureTable::OffCpuRanges)),
            sites: rows_of(table(CaptureTable::Sites)),
            zones: rows_of(table(CaptureTable::Zones)),
            zone_ranges: rows_of(table(CaptureTable::ZoneRanges)),
            plots: rows_of(table(CaptureTable::Plots)),
            markers: rows_of(table(CaptureTable::Markers)),
            tasks: rows_of(table(CaptureTable::Tasks)),
            task_waits: rows_of(table(CaptureTable::TaskWaits)),
        };
        capture.validate().map_err(ImageError::Malformed)?;
        Ok(capture)
    }

    /// Checks every reference, range, and order the tables promise, so that
    /// after it every lookup is in bounds.
    #[expect(
        clippy::too_many_lines,
        reason = "one check per table keeps every promise the tables make in one place"
    )]
    pub fn validate(&self) -> Result<(), String> {
        if !crate::image::valid_pool(&self.strings, true) {
            return Err("the string pool is not NUL-terminated UTF-8".into());
        }
        let string = |id: u32, what: &str| {
            if id == NONE || crate::image::valid_reference(&self.strings, id, true) {
                Ok(())
            } else {
                Err(format!("{what} names no string"))
            }
        };
        let index = |id: u32, len: usize, what: &str| {
            if id == NONE || (id as usize) < len {
                Ok(())
            } else {
                Err(format!("{what} {id} is out of bounds"))
            }
        };
        for setting in &self.settings {
            string(setting.key.get(), "a setting")?;
            string(setting.value.get(), "a setting's value")?;
        }
        for module in &self.modules {
            string(module.path.get(), "a module")?;
            string(module.build_id.get(), "a build id")?;
        }
        for process in &self.processes {
            string(process.name.get(), "a process")?;
        }
        for thread in &self.threads {
            string(thread.name.get(), "a thread")?;
            index(
                thread.process.get(),
                self.processes.len(),
                "a thread's process",
            )?;
        }
        for mapping in &self.mappings {
            index(
                mapping.process.get(),
                self.processes.len(),
                "a mapping's process",
            )?;
            index(
                mapping.module.get(),
                self.modules.len(),
                "a mapping's module",
            )?;
        }
        for frame in &self.frames {
            index(frame.module.get(), self.modules.len(), "a frame's module")?;
            if FrameKind::from_number(frame.kind).is_none() {
                return Err(format!("a frame has unknown kind {}", frame.kind));
            }
        }
        for (position, stack) in self.stacks.iter().enumerate() {
            let parent = stack.parent.get();
            if parent != NONE && parent as usize >= position {
                return Err("a stack's parent does not precede it".into());
            }
            if stack.frame.get() as usize >= self.frames.len() {
                return Err("a stack names no frame".into());
            }
        }
        let grouped = |ranges: &[ThreadRange], rows: &[Sample], what: &str| {
            let mut next = 0;
            for range in ranges {
                index(range.thread.get(), self.threads.len(), what)?;
                if range.thread.get() == NONE || range.start.get() != next {
                    return Err(format!("{what} ranges do not tile their table"));
                }
                next = range
                    .start
                    .get()
                    .checked_add(range.count.get())
                    .ok_or_else(|| format!("{what} range overflows"))?;
                let group = rows
                    .get(range.start.get() as usize..next as usize)
                    .ok_or_else(|| format!("{what} range is out of bounds"))?;
                if group
                    .windows(2)
                    .any(|pair| pair[0].time.get() > pair[1].time.get())
                {
                    return Err(format!("{what} are out of time order"));
                }
            }
            if next as usize != rows.len() {
                return Err(format!("{what} ranges do not cover their table"));
            }
            for sample in rows {
                index(sample.stack.get(), self.stacks.len(), what)?;
                index(sample.task.get(), self.tasks.len(), what)?;
            }
            Ok(())
        };
        grouped(&self.sample_ranges, &self.samples, "samples")?;
        grouped(&self.off_cpu_ranges, &self.off_cpu, "off-CPU intervals")?;
        if !self.frame_symbols.is_empty() && self.frame_symbols.len() != self.frames.len() {
            return Err("symbols do not describe every frame".into());
        }
        for range in &self.frame_symbols {
            let end = u64::from(range.start.get()) + u64::from(range.count.get());
            if end > self.lines.len() as u64 {
                return Err("a frame's lines are out of bounds".into());
            }
        }
        for line in &self.lines {
            if line.function.get() as usize >= self.functions.len() {
                return Err("a line names no function".into());
            }
            string(line.file.get(), "a line's file")?;
        }
        for function in &self.functions {
            string(function.name.get(), "a function")?;
            string(function.file.get(), "a function's file")?;
            index(
                function.module.get(),
                self.modules.len(),
                "a function's module",
            )?;
        }
        for site in &self.sites {
            string(site.name.get(), "a site")?;
            string(site.file.get(), "a site's file")?;
            index(site.module.get(), self.modules.len(), "a site's module")?;
        }
        let mut next = 0;
        for range in &self.zone_ranges {
            index(
                range.thread.get(),
                self.threads.len(),
                "a zone range's thread",
            )?;
            if range.start.get() != next {
                return Err("zone ranges do not tile their table".into());
            }
            next = range
                .start
                .get()
                .checked_add(range.count.get())
                .ok_or("a zone range overflows")?;
            let group = self
                .zones
                .get(range.start.get() as usize..next as usize)
                .ok_or("a zone range is out of bounds")?;
            if group
                .windows(2)
                .any(|pair| pair[0].end.get() > pair[1].start.get())
            {
                return Err("zones of one depth overlap or are out of order".into());
            }
        }
        if next as usize != self.zones.len() {
            return Err("zone ranges do not cover their table".into());
        }
        for zone in &self.zones {
            if zone.site.get() as usize >= self.sites.len() {
                return Err("a zone names no site".into());
            }
            if zone.end.get() < zone.start.get() {
                return Err("a zone ends before it begins".into());
            }
        }
        for plot in &self.plots {
            index(plot.site.get(), self.sites.len(), "a plot's site")?;
            index(plot.thread.get(), self.threads.len(), "a plot's thread")?;
        }
        for marker in &self.markers {
            index(marker.thread.get(), self.threads.len(), "a marker's thread")?;
            index(marker.site.get(), self.sites.len(), "a marker's site")?;
            string(marker.text.get(), "a marker's text")?;
        }
        for task in &self.tasks {
            string(task.runtime.get(), "a task's runtime")?;
            string(task.labels.get(), "a task's labels")?;
            index(task.root.get(), self.frames.len(), "a task's root")?;
        }
        for wait in &self.task_waits {
            index(wait.task.get(), self.tasks.len(), "a wait's task")?;
            index(wait.stack.get(), self.stacks.len(), "a wait's stack")?;
            string(wait.state.get(), "a wait's state")?;
        }
        Ok(())
    }
}

fn rows<T>(rows: &[T], start: u32, count: u32) -> &[T] {
    let start = start as usize;
    &rows[start..start + count as usize]
}

/// A table's rows, copied out of a chunk.
fn rows_of<T: zerocopy::FromBytes + zerocopy::Immutable + Copy>(bytes: &[u8]) -> Vec<T> {
    <[T]>::ref_from_bytes(bytes)
        .map(<[T]>::to_vec)
        .expect("the container checked every table's stride")
}
