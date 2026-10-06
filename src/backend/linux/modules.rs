//! The registry of shared objects mapped into the inferior.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::unistd::Pid;
use object::{Object, ObjectSection, ObjectSegment};

use crate::backend::FileIdentity;
use crate::debug_info::DebugInfo;
use crate::protocol::DebuggerEvent;
use crate::{
    Error, ImageAddress, LoadedModule, LoadedModuleRecord, LoadedModuleSnapshot, ModuleImageId,
    Result, VirtualAddress,
};

use super::native::{InspectionOps, LinuxTraceOps};
use super::vdso::{VDSO_NAME, vdso_mapping};
use super::{Controller, LinuxError, RuntimeModule, backend_error};

/// One file-backed mapping from `/proc/<pid>/maps`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ModuleMapping {
    pub(super) path: PathBuf,
    pub(super) inode: u64,
    pub(super) start: u64,
    pub(super) file_offset: u64,
    /// Whether the file was unlinked or replaced after it was mapped, so the
    /// path may now name a different file or none.
    pub(super) deleted: bool,
    pub(super) executable: bool,
}

impl<P: InspectionOps> Controller<P> {
    pub(super) fn loaded_module(&self) -> Result<LoadedModule> {
        self.inferior
            .as_ref()
            .map(|inferior| inferior.loaded_module)
            .ok_or(Error::NotRunning)
    }

    pub(super) fn loaded_modules(&self) -> Result<LoadedModuleSnapshot> {
        self.inferior.as_ref().ok_or(Error::NotRunning)?;
        Ok(LoadedModuleSnapshot {
            revision: self.revision,
            modules: self
                .modules
                .values()
                .map(|module| LoadedModuleRecord {
                    module: module.loaded,
                    path: Arc::new(module.image.path().to_owned()),
                })
                .collect::<Vec<_>>()
                .into(),
        })
    }

    pub(super) fn unregister_module(&mut self, id: crate::ModuleId) {
        let module = self.modules.remove(&id).expect("registered module exists");
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ModuleUnloaded {
            revision: self.revision,
            module: crate::LoadedModuleRecord {
                module: module.loaded,
                path: Arc::new(module.image.path().to_owned()),
            },
        });
    }

    pub(super) fn reset_runtime_modules(&mut self) {
        let dynamic = self
            .modules
            .keys()
            .copied()
            .filter(|id| *id != crate::ModuleId::new(0))
            .collect::<Vec<_>>();
        for id in dynamic {
            self.unregister_module(id);
        }
        let main = self
            .modules
            .get_mut(&crate::ModuleId::new(0))
            .expect("main module is registered");
        main.loaded = LoadedModule::main(main.image.id(), 0);
        main.link_map = None;
        if self.drop_library_locations() {
            self.bump_revision();
            let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
                revision: self.revision,
            });
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Synchronizes the module registry with the shared objects mapped at a
    /// coherent stop, the vDSO among them. A mapping whose file cannot be
    /// identified, such as a deleted library or JIT code, contributes no
    /// module instead of failing the stop; its frames stay unnamed.
    pub(super) fn refresh_modules(&mut self) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        // The leader may have exited before the rest of its process.
        let pid = inferior.memory_thread();
        let main_loaded = inferior.loaded_module;
        let mut observed = Vec::<(PathBuf, u64)>::new();
        let mut mapped_modules = BTreeMap::new();
        let mappings = self.ptrace.module_mappings(pid)?;
        for mapping in mappings.files {
            if mapping.inode == self.executable_identity.inode {
                continue;
            }
            let module = match self.mapped_modules.remove(&mapping) {
                Some(module) => module,
                None => match self.ptrace.identify_module(&mapping) {
                    Some(module) => module,
                    None => continue,
                },
            };
            if module.0 != *self.executable {
                observed.push(module.clone());
            }
            mapped_modules.insert(mapping, module);
        }
        self.mapped_modules = mapped_modules;
        let (vdso, vdso_image) = self.observe_vdso(pid, mappings.vdso.as_ref()).unzip();
        observed.extend(vdso);
        let mut vdso_image = vdso_image.flatten();
        observed.sort();
        observed.dedup();
        let link_maps = loader_link_maps(&self.ptrace, pid, &self.executable_data, main_loaded)?;

        let observed_modules = observed.iter().cloned().collect::<BTreeSet<_>>();
        let unloaded = self
            .modules
            .iter()
            .filter(|(id, module)| {
                **id != crate::ModuleId::new(0)
                    && !observed_modules
                        .contains(&(module.image.path().to_owned(), module.loaded.load_bias))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in unloaded {
            self.unregister_module(id);
        }

        for (path, load_bias) in observed {
            let existing = self.modules.iter().find_map(|(id, module)| {
                (module.image.path() == path && module.loaded.load_bias == load_bias).then_some(*id)
            });
            if let Some(id) = existing {
                self.modules
                    .get_mut(&id)
                    .expect("observed module exists")
                    .link_map = link_maps.get(&load_bias).copied();
                continue;
            }
            let module_id = crate::ModuleId::new(self.next_module_id);
            self.next_module_id = self
                .next_module_id
                .checked_add(1)
                .ok_or_else(|| backend_error(LinuxError::ModuleIdExhausted))?;
            let image_id = crate::ModuleImageId::new(self.next_image_id);
            self.next_image_id = self
                .next_image_id
                .checked_add(1)
                .ok_or_else(|| backend_error(LinuxError::ModuleImageIdExhausted))?;
            // Metadata a module's file cannot provide leaves its frames unnamed.
            let Ok(debug) = self.load_observed(&path, image_id, &mut vdso_image) else {
                continue;
            };
            let loaded = LoadedModule {
                id: module_id,
                image: image_id,
                load_bias,
            };
            self.modules.insert(
                module_id,
                RuntimeModule {
                    loaded,
                    image: debug.image,
                    unwind: debug.unwind,
                    variables: debug.variables,
                    link_map: link_maps.get(&load_bias).copied(),
                },
            );
            self.bump_revision();
            let _ = self.events.send(DebuggerEvent::ModuleLoaded {
                revision: self.revision,
                module: crate::LoadedModuleRecord {
                    module: loaded,
                    path: Arc::new(path),
                },
            });
        }
        let main = self
            .modules
            .get_mut(&main_loaded.id)
            .expect("main module is registered");
        main.loaded = main_loaded;
        main.link_map = link_maps.get(&main_loaded.load_bias).copied();
        Ok(())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Loads the metadata of an observed module: the vDSO's from the image
    /// read from memory, and any other's from its file.
    fn load_observed(
        &self,
        path: &Path,
        image: ModuleImageId,
        vdso_image: &mut Option<Vec<u8>>,
    ) -> Result<DebugInfo> {
        vdso_image
            .take_if(|_| path.as_os_str() == VDSO_NAME)
            .map_or_else(
                || self.ptrace.load_module(path, image),
                |data| crate::debug_info::load_module_bytes(path, &data, image),
            )
    }
}

pub(super) fn load_bias(
    pid: Pid,
    executable: &Path,
    executable_data: &[u8],
    identity: FileIdentity,
) -> Result<u64> {
    load_bias_in(&read_maps(pid)?, executable, executable_data, identity)
}

/// The load bias of the executable `identity` names in a process whose
/// memory map is `maps`.
pub(super) fn load_bias_in(
    maps: &str,
    executable: &Path,
    executable_data: &[u8],
    identity: FileIdentity,
) -> Result<u64> {
    let object = object::File::parse(executable_data)
        .map_err(|error| Error::backend(LinuxError::Object(error)))?;
    let image_base = object
        .segments()
        .map(|segment| segment.address())
        .min()
        .unwrap_or(0);
    parse_maps(maps)?
        .into_iter()
        .find(|mapping| mapping.inode == identity.inode && mapping.file_offset == 0)
        .and_then(|mapping| mapping.start.checked_sub(image_base))
        .ok_or_else(|| backend_error(LinuxError::LoadBias(executable.to_owned())))
}

/// Reads a process's memory map. A leader that exited before the rest of
/// its process keeps its `/proc` directory but no memory, so its map reads
/// empty, and a live thread's describes the process instead.
pub(super) fn read_maps(pid: Pid) -> std::io::Result<String> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    if !maps.is_empty() {
        return Ok(maps);
    }
    let Ok(threads) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return Ok(maps);
    };
    Ok(threads
        .filter_map(|thread| fs::read_to_string(thread.ok()?.path().join("maps")).ok())
        .find(|maps| !maps.is_empty())
        .unwrap_or(maps))
}

/// Returns the executable mappings of files, which identify loaded modules.
pub(super) fn module_mappings(pid: Pid) -> Result<Vec<ModuleMapping>> {
    Ok(process_mappings_in(&read_maps(pid)?)?.files)
}

/// The mappings of one process that hold loaded modules.
#[derive(Debug, Default)]
pub(super) struct ProcessMappings {
    /// Executable mappings of files.
    pub(super) files: Vec<ModuleMapping>,
    /// The vDSO, which no file backs.
    pub(super) vdso: Option<Range<u64>>,
}

/// The mappings holding loaded modules in a process whose memory map is
/// `maps`.
pub(super) fn process_mappings_in(maps: &str) -> Result<ProcessMappings> {
    let mut files = parse_maps(maps)?;
    files.retain(|mapping| mapping.executable);
    Ok(ProcessMappings {
        files,
        vdso: vdso_mapping(maps),
    })
}

/// Resolves the file behind a module mapping and the load bias it was mapped
/// with. Returns `None` when the file on disk cannot be proven to be the
/// mapped one.
pub(super) fn identify_mapped_module(mapping: &ModuleMapping) -> Option<(PathBuf, u64)> {
    if mapping.deleted {
        return None;
    }
    let path = fs::canonicalize(&mapping.path).ok()?;
    if fs::metadata(&path).ok()?.ino() != mapping.inode {
        return None;
    }
    let bias = mapped_module_load_bias(mapping).ok()?;
    Some((path, bias))
}

/// Each module's loader `link_map`, by load bias, found through the
/// executable's `DT_DEBUG` rendezvous. A program without one has none.
pub(super) fn loader_link_maps(
    ptrace: &impl InspectionOps,
    pid: Pid,
    executable_data: &[u8],
    main: LoadedModule,
) -> Result<BTreeMap<u64, VirtualAddress>> {
    const DYNAMIC_ENTRY_SIZE: u64 = 16;
    const DT_NULL: u64 = 0;
    const DT_DEBUG: u64 = 21;
    const MAX_LINK_MAPS: usize = 1_024;

    let object = object::File::parse(executable_data)
        .map_err(|error| Error::backend(LinuxError::Object(error)))?;
    let Some(dynamic) = object.section_by_name(".dynamic") else {
        return Ok(BTreeMap::new());
    };
    let dynamic_start = main.virtual_address(ImageAddress::new(dynamic.address()))?;
    let entries = dynamic.size() / DYNAMIC_ENTRY_SIZE;
    let mut rendezvous = None;
    for index in 0..entries {
        let address = dynamic_start
            .get()
            .checked_add(index.saturating_mul(DYNAMIC_ENTRY_SIZE))
            .ok_or(Error::AddressOverflow)?;
        let tag = ptrace.read_word(pid, address)?;
        if tag == DT_NULL {
            break;
        }
        if tag == DT_DEBUG {
            rendezvous = Some(VirtualAddress::new(read_word_offset(
                ptrace, pid, address, 8,
            )?));
            break;
        }
    }
    let Some(rendezvous) = rendezvous.filter(|address| address.get() != 0) else {
        return Ok(BTreeMap::new());
    };
    // The public ELF loader rendezvous begins with r_version followed by the
    // aligned r_map pointer. Each public link_map begins with l_addr and ends
    // its debugger-visible prefix with l_next/l_prev.
    let mut current = VirtualAddress::new(read_word_offset(ptrace, pid, rendezvous.get(), 8)?);
    let mut visited = BTreeSet::new();
    let mut result = BTreeMap::new();
    while current.get() != 0 {
        if result.len() == MAX_LINK_MAPS || !visited.insert(current) {
            return Err(backend_error(LinuxError::LoaderRendezvous(
                "link_map traversal exceeded its bound or formed a cycle".to_owned(),
            )));
        }
        let load_bias = ptrace.read_word(pid, current.get())?;
        result.insert(load_bias, current);
        current = VirtualAddress::new(read_word_offset(ptrace, pid, current.get(), 24)?);
    }
    Ok(result)
}

fn read_word_offset(ptrace: &impl InspectionOps, pid: Pid, base: u64, offset: u64) -> Result<u64> {
    ptrace.read_word(pid, base.checked_add(offset).ok_or(Error::AddressOverflow)?)
}

/// Parses the file-backed mappings of `/proc/<pid>/maps`, sorted and
/// deduplicated.
pub(super) fn parse_maps(maps: &str) -> Result<Vec<ModuleMapping>> {
    let mut mappings = Vec::new();
    for line in maps.lines() {
        let invalid = || backend_error(LinuxError::InvalidMapping(line.to_owned()));
        // The pathname is the sixth field and may itself contain spaces, so it
        // must be taken as the untouched remainder of the line rather than one
        // whitespace-delimited token. The kernel escapes control characters but
        // not spaces in this field.
        let mut fields = line.splitn(6, char::is_whitespace);
        let (Some(range), Some(permissions), Some(offset), Some(_device), Some(inode)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(invalid());
        };
        let Some(path) = fields.next().map(str::trim_start) else {
            continue;
        };
        if inode == "0" || !path.starts_with('/') {
            continue;
        }
        let (path, deleted) = path
            .strip_suffix(" (deleted)")
            .map_or((path, false), |path| (path, true));
        mappings.push(ModuleMapping {
            path: PathBuf::from(path),
            inode: inode.parse().map_err(|_| invalid())?,
            start: range
                .split_once('-')
                .and_then(|(start, _)| u64::from_str_radix(start, 16).ok())
                .ok_or_else(invalid)?,
            file_offset: u64::from_str_radix(offset, 16).map_err(|_| invalid())?,
            deleted,
            executable: permissions.contains('x'),
        });
    }
    mappings.sort();
    mappings.dedup();
    Ok(mappings)
}

pub(super) fn mapped_module_load_bias(mapping: &ModuleMapping) -> Result<u64> {
    const PAGE_MASK: u64 = !0xfff;
    let data = fs::read(&mapping.path)?;
    let object = object::File::parse(data.as_slice())
        .map_err(|error| Error::backend(LinuxError::Object(error)))?;
    for segment in object.segments() {
        let (file_offset, _) = segment.file_range();
        if file_offset & PAGE_MASK != mapping.file_offset {
            continue;
        }
        let image_start = segment.address() & PAGE_MASK;
        return mapping
            .start
            .checked_sub(image_start)
            .ok_or_else(|| backend_error(LinuxError::LoadBias(mapping.path.clone())));
    }
    Err(backend_error(LinuxError::LoadBias(mapping.path.clone())))
}
