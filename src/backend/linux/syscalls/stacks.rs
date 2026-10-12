//! The stacks of system calls (`-k`).
//!
//! Each traced process's modules come from the records of its mappings or
//! from `/proc`. A stack is unwound through them by call-frame information,
//! and each of its addresses is named by module, symbol, and source line.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use foldhash::HashMap;

use super::super::perf::{self, decode};
use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::syscalls::CallStack;
use crate::unwind::sample::{
    FrameRules, SampledFrame, SampledState, StackEnd, unwind_memory, unwind_sample,
};
use crate::unwind::{MemoryReader, UnwindModule};
use crate::{
    CodeRole, DebugFileOptions, ImageAddress, LoadedModule, ModuleId, ModuleImageId, VirtualAddress,
};

/// The most frames a stack keeps, as strace's default.
pub const MOST_FRAMES: usize = 256;

/// One module mapped into a process.
#[derive(Clone)]
struct Mapped {
    start: u64,
    end: u64,
    path: Arc<str>,
    loaded: LoadedModule,
    info: Option<Arc<DebugInfo>>,
}

/// One address of a stack, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The code the frame runs: the innermost frame's instruction, and in
    /// each frame after, the byte before its return address, inside the
    /// call, as strace names it.
    pub address: u64,
    /// The mapped file, as `/proc/PID/maps` names it.
    pub module: Option<Arc<str>>,
    /// The address within the module's image.
    pub image_address: u64,
    /// The function's symbol and the address's offset from it.
    pub symbol: Option<(Arc<str>, u64)>,
    /// The source file and line of the code the frame runs.
    pub source: Option<(String, u64)>,
}

/// Each traced process's modules, and the images they map, loaded once.
///
/// A process's modules come from the records of its mappings, as they
/// happen, when an engine opens them; otherwise from `/proc`, which a
/// process that has ended no longer has.
pub struct Stacks {
    search: DebugFileSearch,
    images: HashMap<(PathBuf, u64), Option<Arc<DebugInfo>>>,
    processes: BTreeMap<u32, Vec<Mapped>>,
    rules: FrameRules,
    next_module: u32,
    /// The vDSO's image, read from uscope's own, which the kernel maps
    /// alike into every process, and the address it links at.
    vdso: Vdso,
}

impl std::fmt::Debug for Stacks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Stacks")
            .field("processes", &self.processes.len())
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

/// Whether the vDSO's image has been read.
#[derive(Clone)]
enum Vdso {
    Unread,
    Read((Arc<DebugInfo>, u64)),
    Unreadable,
}

/// Stacks shared by an engine, which unwinds them, and whatever prints
/// them, so that each image loads once.
pub type SharedStacks = Arc<std::sync::Mutex<Stacks>>;

impl Default for Stacks {
    fn default() -> Self {
        Self::new(&DebugFileOptions::default())
    }
}

/// A live thread's memory, read from outside it.
struct Live(u32);

impl MemoryReader for Live {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let mut word = [0; 8];
        (perf::read_memory(self.0, address.get(), &mut word).ok()? == 8)
            .then(|| u64::from_le_bytes(word))
    }
}

impl Stacks {
    #[must_use]
    pub fn new(options: &DebugFileOptions) -> Self {
        Self {
            search: DebugFileSearch::new(options),
            images: HashMap::default(),
            processes: BTreeMap::new(),
            rules: FrameRules::default(),
            next_module: 0,
            vdso: Vdso::Unread,
        }
    }

    /// A mapping the process made, from its record.
    pub fn mapped(&mut self, mmap: &decode::Mmap<'_>) {
        let end = mmap.start.saturating_add(mmap.length);
        let process = self.processes.entry(mmap.pid).or_default();
        process.retain(|mapped| mapped.end <= mmap.start || mapped.start >= end);
        if let Some(mapped) = self.module(mmap) {
            self.processes.entry(mmap.pid).or_default().push(mapped);
        }
    }

    /// The module a mapping maps, if it is a file's code or the vDSO.
    fn module(&mut self, mmap: &decode::Mmap<'_>) -> Option<Mapped> {
        let (info, bias) = if perf::is_vdso(mmap) {
            let (info, link) = self.vdso()?;
            (Some(info), mmap.start.wrapping_sub(link))
        } else {
            let (file, bias) = perf::identify(mmap)?;
            (self.image(&file, mmap.inode.unwrap_or(0)), bias)
        };
        let id = info
            .as_ref()
            .map_or(ModuleImageId::new(u32::MAX), |info| info.image.id());
        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: id,
            load_bias: bias,
        };
        self.next_module += 1;
        Some(Mapped {
            start: mmap.start,
            end: mmap.start.saturating_add(mmap.length),
            path: String::from_utf8_lossy(mmap.path).into(),
            loaded,
            info,
        })
    }

    fn vdso(&mut self) -> Option<(Arc<DebugInfo>, u64)> {
        match &self.vdso {
            Vdso::Read(vdso) => return Some(vdso.clone()),
            Vdso::Unreadable => return None,
            Vdso::Unread => {}
        }
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let vdso = perf::own_vdso().and_then(|(data, link)| {
            let path = std::path::Path::new(super::super::vdso::VDSO_NAME);
            let info = crate::debug_info::load_module_bytes(path, &data, id, &self.search).ok()?;
            Some((Arc::new(info), link))
        });
        self.vdso = vdso.clone().map_or(Vdso::Unreadable, Vdso::Read);
        vdso
    }

    /// Forgets a process's modules, as after it replaced its image: they
    /// are read again when a stack next needs them.
    pub fn forget(&mut self, process: u32) {
        self.processes.remove(&process);
    }

    /// Unwinds a stopped thread's stack from its registers by DWARF
    /// number, reading its memory: its instruction, then each return
    /// address, innermost first.
    pub fn unwind_live(
        &mut self,
        process: u32,
        thread: u32,
        registers: &[(u16, u64)],
    ) -> CallStack {
        let instruction = registers.iter().find_map(|&(register, value)| {
            (register == crate::unwind::RETURN_ADDRESS).then_some(value)
        });
        self.cover(process, instruction);
        let modules = self.modules(process);
        let (frames, end) = unwind_memory(&modules, registers, &mut Live(thread), MOST_FRAMES);
        stack(&frames, end)
    }

    /// Unwinds a stack the kernel sampled: the registers, a copy of the
    /// stack from the stack pointer up, and the kernel's frame-pointer
    /// walk, instruction first.
    pub fn unwind_sampled(&mut self, process: u32, sample: SampledState<'_>) -> CallStack {
        self.cover(process, sample.chain.first().copied());
        let modules: Vec<UnwindModule<'_>> = self
            .processes
            .get(&process)
            .into_iter()
            .flatten()
            .filter_map(|mapped| {
                let info = mapped.info.as_deref()?;
                Some(UnwindModule {
                    loaded: mapped.loaded,
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect();
        let (frames, end) = unwind_sample(&modules, &mut self.rules, sample, MOST_FRAMES);
        stack(&frames, end)
    }

    /// Names each address of a stack of `process`'s.
    pub fn frames(&mut self, process: u32, stack: &[u64]) -> Vec<Frame> {
        if let Some(&first) = stack.first() {
            self.cover(process, Some(first));
        }
        stack
            .iter()
            .enumerate()
            .map(|(index, &address)| self.frame(process, address, index == 0))
            .collect()
    }

    fn frame(&self, process: u32, address: u64, innermost: bool) -> Frame {
        // A return address may follow a call that never returns, past the
        // end of its function: the call itself names the caller's code.
        let code = if innermost {
            address
        } else {
            address.wrapping_sub(1)
        };
        let mapped = self.mapping_at(process, code);
        let Some(mapped) = mapped else {
            return Frame {
                address: code,
                module: None,
                image_address: code,
                symbol: None,
                source: None,
            };
        };
        let code_address = ImageAddress::new(code.wrapping_sub(mapped.loaded.load_bias));
        let (symbol, source) = mapped.info.as_deref().map_or((None, None), |info| {
            let image = &info.image;
            let symbol = image.symbolize(code_address).map(|found| {
                let name = image
                    .symbol(found.symbol)
                    .and_then(crate::Symbol::demangled_name)
                    .map_or(found.name, Arc::from);
                (name, found.offset)
            });
            let source = image.source_location(code_address).and_then(|location| {
                let file = image.source_file(location.file)?;
                Some((file.path.display().to_string(), location.line.get()))
            });
            (symbol, source)
        });
        Frame {
            address: code,
            module: Some(Arc::clone(&mapped.path)),
            image_address: code_address.get(),
            symbol,
            source,
        }
    }

    fn mapping_at(&self, process: u32, address: u64) -> Option<Mapped> {
        self.processes
            .get(&process)?
            .iter()
            .find(|mapped| mapped.start <= address && address < mapped.end)
            .cloned()
    }

    fn modules(&self, process: u32) -> Vec<UnwindModule<'_>> {
        self.processes
            .get(&process)
            .into_iter()
            .flatten()
            .filter_map(|mapped| {
                let info = mapped.info.as_deref()?;
                Some(UnwindModule {
                    loaded: mapped.loaded,
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect()
    }

    /// A process forked from another, whose mappings it begins with.
    pub fn forked(&mut self, parent: u32, child: u32) {
        if let Some(mapped) = self.processes.get(&parent).cloned() {
            self.processes.insert(child, mapped);
        }
    }

    /// Reads what the process maps now from `/proc`.
    pub fn read_maps(&mut self, process: u32) {
        self.processes.remove(&process);
        self.cover(process, None);
    }

    /// Reads the process's modules again unless they map `address`.
    fn cover(&mut self, process: u32, address: Option<u64>) {
        let known = self.processes.get(&process).is_some_and(|mapped| {
            address.is_none_or(|address| {
                mapped
                    .iter()
                    .any(|mapped| mapped.start <= address && address < mapped.end)
            })
        });
        if known {
            return;
        }
        let Ok(mappings) = perf::mappings(process) else {
            return;
        };
        let mut modules = Vec::new();
        for mapping in mappings {
            let mmap = decode::Mmap {
                pid: process,
                start: mapping.start,
                length: mapping.end - mapping.start,
                file_offset: mapping.file_offset,
                inode: Some(mapping.inode),
                executable: mapping.executable,
                path: &mapping.path,
                time: 0,
            };
            modules.extend(self.module(&mmap));
        }
        self.processes.insert(process, modules);
    }

    fn image(&mut self, file: &std::path::Path, inode: u64) -> Option<Arc<DebugInfo>> {
        let key = (file.to_path_buf(), inode);
        if let Some(info) = self.images.get(&key) {
            return info.clone();
        }
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let info = crate::image::backing::read_input(file)
            .ok()
            .and_then(|(data, _)| {
                crate::debug_info::load_module_bytes(file, &data, id, &self.search).ok()
            })
            .map(Arc::new);
        self.images.insert(key, info.clone());
        info
    }
}

/// The stack of unwound frames: the innermost frame's instruction, and for
/// each frame after, the return address its call left, unless a signal
/// interrupted it at the instruction itself.
fn stack(frames: &[SampledFrame], end: StackEnd) -> CallStack {
    let addresses = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let interrupted = index == 0
                || frame.role == CodeRole::SignalTrampoline
                || frames[index - 1].role == CodeRole::SignalTrampoline;
            if interrupted {
                frame.address.get()
            } else {
                frame.address.get().wrapping_add(1)
            }
        })
        .collect();
    CallStack {
        addresses,
        truncated: match end {
            StackEnd::Complete => None,
            StackEnd::Truncated(reason, _) => Some(reason as u8),
        },
    }
}
