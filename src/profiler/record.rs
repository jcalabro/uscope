//! Turns the records perf events write into a capture: it follows each
//! process's mappings and threads, unwinds each sample through the modules
//! mapped when it was taken, and symbolizes the frames once at the end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use foldhash::HashMap;

use super::calls::Branch;
use super::frame_pointers::{CallSites, call_sites};
use crate::backend::perf::{self, decode};
use crate::capture::builder::LineInput;
use crate::capture::records::{AccountKind, FrameKind, Mapping, NONE, Truncation, role_number};
use crate::capture::{Capture, CaptureBuilder};
use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::unwind::UnwindModule;
use crate::unwind::sample::{FrameRules, SampledState, StackEnd, unwind_sample};
use crate::{
    CodeInstanceKind, CodeRole, ImageAddress, InlineFrameLookup, LoadedModule, ModuleId,
    ModuleImage, ModuleImageId,
};

/// How a program's call sites keep the frame-pointer chain.
pub enum Sites {
    /// It is linked dynamically, so its code is not all in its file.
    Dynamic,
    /// Its own file's, with the function of its first call that does not.
    Static(CallSites, Option<String>),
}

/// A module file the recording mapped, loaded once for every mapping.
struct Image {
    /// `None` when its debug information could not be read: its frames
    /// are known by address only.
    info: Option<DebugInfo>,
    id: ModuleImageId,
    /// The capture's module.
    module: u32,
}

/// One executable mapping of a process.
#[derive(Debug, Clone, Copy)]
struct Mapped {
    start: u64,
    end: u64,
    image: usize,
    loaded: LoadedModule,
}

#[derive(Debug, Clone, Default)]
struct ProcessState {
    mapped: Vec<Mapped>,
    threads: BTreeSet<u32>,
}

/// Builds a capture from perf records as they are drained.
pub struct Processor {
    builder: CaptureBuilder,
    rules: FrameRules,
    search: DebugFileSearch,
    images: Vec<Image>,
    image_index: HashMap<(PathBuf, u64), usize>,
    /// Whether the vDSO's image has been read, and if it could be, the
    /// image and the address it links at.
    vdso_read: bool,
    vdso: Option<(usize, u64)>,
    processes: BTreeMap<u32, ProcessState>,
    next_module: u32,
    limit: usize,
    /// Records held back for the next batch, past its watermark.
    carry: Vec<u8>,
    /// The latest time processed.
    latest: u64,
    first: Option<u64>,
    order: Vec<(u64, usize, usize)>,
    registers: Vec<(u16, u64)>,
    /// A breakpoint's instruction and the branch that makes its samples
    /// repeats, not calls.
    repeats: Option<(u64, Branch)>,
    chain: Vec<u64>,
    frames: Vec<u32>,
}

impl Processor {
    pub fn new(search: DebugFileSearch, limit: usize) -> Self {
        Self {
            builder: CaptureBuilder::new(),
            rules: FrameRules::default(),
            search,
            images: Vec::new(),
            image_index: HashMap::default(),
            vdso_read: false,
            vdso: None,
            processes: BTreeMap::new(),
            next_module: 0,
            limit,
            carry: Vec::new(),
            latest: 0,
            first: None,
            order: Vec::new(),
            registers: Vec::with_capacity(decode::DWARF_REGISTERS.len()),
            repeats: None,
            chain: Vec::new(),
            frames: Vec::new(),
        }
    }

    pub const fn builder(&mut self) -> &mut CaptureBuilder {
        &mut self.builder
    }

    /// How the program at `path` keeps the frame-pointer chain, or `None`
    /// if it cannot be read. Its image is loaded as its mapping will find
    /// it, and loaded once.
    pub fn call_sites(&mut self, path: &std::path::Path) -> Option<Sites> {
        use std::os::unix::fs::MetadataExt;
        let data = std::fs::read(path).ok()?;
        let file = object::File::parse(data.as_slice()).ok()?;
        if object::Object::section_by_name(&file, ".interp").is_some() {
            return Some(Sites::Dynamic);
        }
        let inode = std::fs::metadata(path).ok()?.ino();
        let image = self.image(path.to_owned(), Some(inode));
        let info = self.images[image].info.as_ref()?;
        let sites = call_sites(&data, &info.image, info.unwind.as_ref());
        let function = sites.first_not_kept.and_then(|address| {
            info.image
                .symbols()
                .take_while(|symbol| symbol.address().get() <= address)
                .last()
                .map(|symbol| {
                    crate::demangle::qualified_name(symbol.name())
                        .unwrap_or_else(|| symbol.name().to_owned())
                })
        });
        Some(Sites::Static(sites, function))
    }

    /// Drops the samples at the instruction at `address` that `branch`
    /// takes, counting them as repeats.
    pub const fn repeats_when(&mut self, address: u64, branch: Branch) {
        self.repeats = Some((address, branch));
    }

    /// Takes the events' count of samples lost so far.
    pub fn lost(&mut self, total: u64) {
        let counted = self.builder.account(AccountKind::LostSamples as u32);
        self.builder.count(
            AccountKind::LostSamples as u32,
            total.saturating_sub(counted),
        );
    }

    /// Processes the records in `batch` and those held back before, in
    /// time order, up to `watermark`; holds back the rest. Rings are
    /// drained one after another, so a record of one may be written after
    /// a later record of another was drained: the watermark lies far
    /// enough behind the drain that every record before it has arrived,
    /// and one that still comes later is counted.
    pub fn ingest(&mut self, batch: &[u8], watermark: Option<u64>) {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(batch);
        self.order.clear();
        let mut offset = 0;
        while let Some(next) = decode::next(&bytes[offset..]) {
            let Ok((record, length)) = next else {
                self.builder.count(AccountKind::Malformed as u32, 1);
                break;
            };
            let time = record.time().unwrap_or(self.latest);
            self.order.push((time, offset, length));
            offset += length;
        }
        self.order.sort_by_key(|(time, offset, _)| (*time, *offset));
        let order = std::mem::take(&mut self.order);
        for &(time, offset, length) in &order {
            let record = &bytes[offset..offset + length];
            if watermark.is_some_and(|watermark| time > watermark) {
                self.carry.extend_from_slice(record);
                continue;
            }
            if let Some(Ok((record, _))) = decode::next(record) {
                self.process(record);
            }
        }
        self.order = order;
    }

    fn process(&mut self, record: decode::Record<'_>) {
        let time = record.time().unwrap_or(self.latest);
        if time < self.latest {
            self.builder.count(AccountKind::LateRecords as u32, 1);
        }
        self.latest = self.latest.max(time);
        self.first.get_or_insert(time);
        let at = time.cast_signed();
        match record {
            decode::Record::Sample(sample) => self.sample(&sample),
            decode::Record::Mmap(mmap) => self.mmap(&mmap),
            decode::Record::Comm {
                pid,
                tid,
                name,
                exec,
                ..
            } => {
                let name = String::from_utf8_lossy(name);
                let thread = self.thread(pid, tid, at);
                self.builder.thread_name(thread, &name);
                if exec {
                    // A new program replaces the address space.
                    self.processes.entry(pid).or_default().mapped.clear();
                    let process = self.builder.process(pid, at);
                    self.builder.process_name(process, &name);
                }
            }
            decode::Record::Fork { pid, ppid, tid, .. } => {
                if pid == tid && pid != ppid {
                    // A forked process starts with its parent's mappings.
                    let mapped = self
                        .processes
                        .get(&ppid)
                        .map(|parent| parent.mapped.clone())
                        .unwrap_or_default();
                    self.processes.entry(pid).or_default().mapped = mapped;
                }
                self.thread(pid, tid, at);
            }
            decode::Record::Exit { pid, tid, .. } => {
                self.builder.thread_exit(tid, at);
                let ended = self.processes.get_mut(&pid).is_some_and(|process| {
                    process.threads.remove(&tid);
                    process.threads.is_empty()
                });
                if ended {
                    self.processes.remove(&pid);
                    self.builder.process_exit(pid, at);
                }
            }
            // A loss record says only what its ring lost since it last had
            // room, and a ring that never has room again never says: the
            // events' own counts are exact, and `lost` takes them.
            decode::Record::Throttle { .. } => {
                self.builder.count(AccountKind::Throttles as u32, 1);
            }
            decode::Record::Lost { .. } | decode::Record::Other { .. } => {}
        }
    }

    fn thread(&mut self, pid: u32, tid: u32, time: i64) -> u32 {
        self.processes.entry(pid).or_default().threads.insert(tid);
        self.builder.thread(pid, tid, time)
    }

    fn mmap(&mut self, mmap: &decode::Mmap<'_>) {
        let end = mmap.start.saturating_add(mmap.length);
        let process = self.builder.process(mmap.pid, mmap.time.cast_signed());
        let image = if perf::is_vdso(mmap) {
            self.vdso()
                .map(|(image, link)| (image, mmap.start.wrapping_sub(link)))
        } else {
            perf::identify(mmap).map(|(path, bias)| (self.image(path, mmap.inode), bias))
        };
        let state = self.processes.entry(mmap.pid).or_default();
        // A mapping replaces whatever it overlaps.
        state
            .mapped
            .retain(|mapped| mapped.end <= mmap.start || mapped.start >= end);
        let Some((image, bias)) = image else {
            return;
        };
        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: self.images[image].id,
            load_bias: bias,
        };
        self.next_module += 1;
        state.mapped.push(Mapped {
            start: mmap.start,
            end,
            image,
            loaded,
        });
        self.builder.mapping(Mapping {
            process: process.into(),
            module: self.images[image].module.into(),
            start: mmap.start.into(),
            end: end.into(),
            file_offset: mmap.file_offset.into(),
            bias: bias.into(),
            time: mmap.time.cast_signed().into(),
        });
    }

    /// The vDSO's image, which no file holds, and the address it links at.
    fn vdso(&mut self) -> Option<(usize, u64)> {
        if self.vdso_read {
            return self.vdso;
        }
        self.vdso_read = true;
        let vdso = perf::own_vdso().map(|(data, link)| {
            (
                self.add_image(std::path::Path::new("[vdso]"), Some(data)),
                link,
            )
        });
        self.vdso = vdso;
        vdso
    }

    /// The image of the file at `path`, loading it the first time.
    fn image(&mut self, path: PathBuf, inode: Option<u64>) -> usize {
        let key = (path, inode.unwrap_or(0));
        if let Some(&image) = self.image_index.get(&key) {
            return image;
        }
        let data = crate::image::backing::read_input(&key.0)
            .ok()
            .map(|(data, _)| data);
        let image = self.add_image(&key.0, data);
        self.image_index.insert(key, image);
        image
    }

    /// Adds the image of a module whose file holds `data`, if it could be
    /// read.
    fn add_image(&mut self, path: &std::path::Path, data: Option<Vec<u8>>) -> usize {
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let build_id = data.as_ref().and_then(|data| {
            object::File::parse(data.as_slice())
                .ok()
                .and_then(|file| object::Object::build_id(&file).ok().flatten().map(hex))
        });
        let digest = data.as_deref().map_or([0; 16], |data| {
            twox_hash::XxHash3_128::oneshot(data).to_le_bytes()
        });
        let size = data.as_ref().map_or(0, |data| data.len() as u64);
        let module =
            self.builder
                .module(&path.to_string_lossy(), build_id.as_deref(), digest, size);
        let info = data.and_then(|data| {
            crate::debug_info::load_module_bytes(path, &data, id, &self.search).ok()
        });
        let image = self.images.len();
        self.images.push(Image { info, id, module });
        image
    }

    fn sample(&mut self, sample: &decode::Sample<'_>) {
        let time = sample.time.cast_signed();
        let thread = self.thread(sample.pid, sample.tid, time);
        self.registers.clear();
        self.registers.extend(
            decode::DWARF_REGISTERS
                .iter()
                .copied()
                .zip(sample.registers.iter()),
        );
        if let Some((address, branch)) = self.repeats {
            let register = |number| {
                self.registers
                    .iter()
                    .find_map(|(register, value)| (*register == number).then_some(*value))
            };
            let at = register(crate::unwind::RETURN_ADDRESS) == Some(address);
            if at && register(decode::DWARF_FLAGS).is_some_and(|flags| branch.taken(flags)) {
                self.builder.count(AccountKind::Repeats as u32, 1);
                return;
            }
        }
        self.chain.clear();
        self.chain.extend(sample.user_chain());
        self.builder
            .count(AccountKind::StackBytes as u32, sample.stack.len() as u64);

        let mapped: &[Mapped] = self
            .processes
            .get(&sample.pid)
            .map_or(&[], |process| &process.mapped);
        let mut owners = Vec::with_capacity(mapped.len());
        let modules: Vec<UnwindModule<'_>> = mapped
            .iter()
            .filter_map(|mapped| {
                let info = self.images[mapped.image].info.as_ref()?;
                owners.push(*mapped);
                Some(UnwindModule {
                    loaded: mapped.loaded,
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect();
        let (frames, end) = unwind_sample(
            &modules,
            &mut self.rules,
            SampledState {
                registers: &self.registers,
                stack: sample.stack,
                chain: &self.chain,
            },
            self.limit,
        );
        self.frames.clear();
        for frame in &frames {
            let id = match frame.module.map(|module| owners[module]) {
                Some(owner) => self.builder.frame(
                    FrameKind::Native,
                    self.images[owner.image].module,
                    frame.address.get().wrapping_sub(owner.loaded.load_bias),
                    role_number(frame.role),
                ),
                None => self
                    .builder
                    .frame(FrameKind::Native, NONE, frame.address.get(), 0),
            };
            self.frames.push(id);
        }
        if let StackEnd::Truncated(reason, module) = end {
            let module = module.map_or(NONE, |module| self.images[owners[module].image].module);
            let frame = self.builder.truncation(reason, module);
            self.frames.push(frame);
            self.builder
                .count(AccountKind::Truncated as u32 + reason as u32, 1);
        }
        let stack = self.builder.stack(&self.frames);
        self.builder
            .sample(thread, time, sample.period, stack, NONE);
    }

    /// The capture so far, every frame symbolized, which goes on growing:
    /// what a live view shows. Records held back stay held back.
    pub fn snapshot(&mut self) -> Capture {
        self.symbolize_new();
        self.builder.clone().finish()
    }

    /// The finished capture, every frame symbolized.
    pub fn finish(mut self) -> Capture {
        // Everything held back is processed now.
        self.ingest(&[], None);
        self.symbolize_new();
        self.builder.finish()
    }

    /// Gives each frame not yet symbolized its lines, and the capture its
    /// span so far.
    fn symbolize_new(&mut self) {
        let start = self.first.unwrap_or(0);
        self.builder.setting("start", &start.to_string());
        self.builder.setting("end", &self.latest.to_string());
        let images: BTreeMap<u32, usize> = self
            .images
            .iter()
            .enumerate()
            .map(|(index, image)| (image.module, index))
            .collect();
        let frames: Vec<(u32, u32, u64)> = self
            .builder
            .frames()
            .iter()
            .enumerate()
            .filter(|(index, frame)| {
                frame.kind == FrameKind::Native as u8
                    && frame.module.get() != NONE
                    && !self
                        .builder
                        .symbolized(u32::try_from(*index).unwrap_or(u32::MAX))
            })
            .map(|(index, frame)| {
                (
                    u32::try_from(index).unwrap_or(u32::MAX),
                    frame.module.get(),
                    frame.address.get(),
                )
            })
            .collect();
        for (frame, module, address) in frames {
            let Some(info) = images
                .get(&module)
                .and_then(|&image| self.images[image].info.as_ref())
            else {
                continue;
            };
            let lines = symbolize(&info.image, ImageAddress::new(address));
            let inputs: Vec<LineInput<'_>> = lines.iter().map(OwnedLine::input).collect();
            self.builder.symbolize(frame, module, &inputs);
        }
    }
}

/// One source line of a frame, owned until the builder interns it.
struct OwnedLine {
    function: Arc<str>,
    file: Option<String>,
    line: u32,
    column: u32,
    declared: Option<(String, u32)>,
    role: CodeRole,
}

impl OwnedLine {
    fn input(&self) -> LineInput<'_> {
        LineInput {
            function: &self.function,
            file: self.file.as_deref(),
            line: self.line,
            column: self.column,
            declared: self
                .declared
                .as_ref()
                .map(|(file, line)| (file.as_str(), *line)),
            role: role_number(self.role),
        }
    }
}

/// The source lines of the code at `address`, innermost inlined function
/// first, ending with the function the code physically belongs to.
fn symbolize(image: &ModuleImage, address: ImageAddress) -> Vec<OwnedLine> {
    let location = image.locate(address);
    let path = |file| {
        image
            .source_file(file)
            .map(|file: &crate::SourceFile| file.path.display().to_string())
    };
    let line = |function: Arc<str>,
                role: CodeRole,
                declaration: Option<&crate::SourceLocation>,
                source: Option<&crate::SourceLocation>| OwnedLine {
        function,
        role,
        file: source.and_then(|source| path(source.file)),
        line: source.map_or(0, |source| {
            u32::try_from(source.line.get()).unwrap_or(u32::MAX)
        }),
        column: source
            .and_then(|source| source.column)
            .map_or(0, |column| u32::try_from(column.get()).unwrap_or(u32::MAX)),
        declared: declaration.and_then(|declaration| {
            Some((
                path(declaration.file)?,
                u32::try_from(declaration.line.get()).unwrap_or(u32::MAX),
            ))
        }),
    };
    let mut lines = Vec::new();
    let mut source = location.source.clone();
    if let InlineFrameLookup::Unique(chain) = &location.inline_frames {
        for &instance in chain.instances.iter().rev() {
            let Some(instance) = image.code_instance(instance) else {
                continue;
            };
            if let Some(function) = image.function(instance.function()) {
                let function = function.info();
                lines.push(line(
                    qualified(&function),
                    function.role,
                    function.declaration.as_ref(),
                    source.as_ref(),
                ));
            }
            source = match instance.kind() {
                CodeInstanceKind::Inline { call_site } => call_site,
                CodeInstanceKind::OutOfLine => None,
            };
        }
    }
    let physical = location
        .physical_instance
        .and_then(|instance| image.code_instance(instance))
        .and_then(|instance| image.function(instance.function()))
        .map(crate::Function::info);
    if let Some(function) = physical {
        lines.push(line(
            qualified(&function),
            function.role,
            function.declaration.as_ref(),
            source.as_ref(),
        ));
    } else if let Some(symbol) = &location.symbol {
        let found = image.symbol(symbol.symbol);
        let name = found
            .and_then(crate::Symbol::demangled_name)
            .map_or_else(|| symbol.name.clone(), Arc::from);
        let role = found.map_or(CodeRole::Ordinary, crate::Symbol::role);
        lines.push(line(name, role, None, source.as_ref()));
    }
    lines
}

/// A function's name with the scopes that qualify it, as its linkage name
/// spells them: a profile names every function of a program side by side.
fn qualified(function: &crate::FunctionInfo) -> Arc<str> {
    function
        .linkage_name
        .as_deref()
        .and_then(crate::demangle::qualified_name)
        .map_or_else(|| function.name.clone(), Arc::from)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Truncations by number, for the accounts that count them.
pub fn truncation_counts(capture: &Capture) -> Vec<(Truncation, u64)> {
    Truncation::ALL
        .into_iter()
        .map(|reason| {
            (
                reason,
                capture.account(AccountKind::Truncated as u32 + reason as u32),
            )
        })
        .filter(|(_, count)| *count > 0)
        .collect()
}
