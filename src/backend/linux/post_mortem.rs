//! Post-mortem sessions present a core dump as one permanent all-stop.
//!
//! Every inspection request shares the live controller's implementation over
//! [`CoreTarget`]. Requests that would execute, modify, or trap the target are
//! rejected before reaching any process-control state.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use nix::libc;
use nix::sys::signal::Signal as NixSignal;
use nix::unistd::Pid;
use object::Object as _;

use super::core_dump::{
    AT_ENTRY, AT_PHDR, CoreDump, CoreError, CoreMemory, CoreMemoryError, CoreSignal, CoreThread,
    FileBacking, ImageEvidence, ImageMappings, ImageVerification, SavedHeader, image_mappings,
    is_elf, recorded_build_id, saved_header, verify_image,
};
use super::core_files::{ModuleFile, ModuleLocator, hex, open_explicit};
use super::thread_db::{self, ProcessServices};
use super::{
    Controller, ControllerChannels, ExecutableSource, ExpectedStop, FileIdentity, Fxsave, Inferior,
    InferiorOrigin, InspectionOps, LinuxError, MemoryAccessError, NativeThreadState, PublicStop,
    Reply, RuntimeModule, SessionLease, TraceThread, allocate_stop_id, backend_error,
    loader_link_maps,
};
use crate::backend::ControllerMessage;
use crate::protocol::{
    CoreDumpInfo, CoreDumpOptions, CoreModule, CoreModuleState, ExceptionInfo, ModuleIdentity,
    ProcessId, Request, StopReason,
};
use crate::{
    Error, LoadedModule, LoadedModuleRecord, ModuleId, ModuleImage, ModuleImageId, Result,
    VirtualAddress,
};

const POST_MORTEM_THREAD_NAME: &str = "uscope-core";

/// Registers and memory recorded by a core dump.
pub(super) struct CoreTarget {
    memory: CoreMemory,
    /// Loaded images whose symbols answer `libthread_db` lookups.
    symbol_modules: Vec<SymbolModule>,
}

struct SymbolModule {
    recorded_path: PathBuf,
    load_bias: u64,
    image: Arc<ModuleImage>,
}

impl ProcessServices for CoreTarget {
    fn read(&self, address: u64, output: &mut [u8]) -> bool {
        self.memory.read(address, output).is_ok()
    }

    fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct> {
        self.thread(lwp).ok().map(|thread| thread.registers)
    }

    /// Prefers the named object, as the live lookup does, but accepts the
    /// symbol from any loaded module when the object name differs.
    fn lookup_symbol(&self, object: &str, symbol: &str) -> Option<u64> {
        let mut fallback = None;
        for module in &self.symbol_modules {
            let Some(found) = module
                .image
                .symbols()
                .iter()
                .find(|candidate| candidate.name.as_ref() == symbol)
            else {
                continue;
            };
            let Some(address) = module.load_bias.checked_add(found.address.get()) else {
                continue;
            };
            let preferred = object.is_empty()
                || module
                    .recorded_path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(object));
            if preferred {
                return Some(address);
            }
            fallback.get_or_insert(address);
        }
        fallback
    }
}

impl CoreTarget {
    fn thread(&self, pid: Pid) -> Result<&CoreThread> {
        u32::try_from(pid.as_raw())
            .ok()
            .and_then(|tid| self.memory.core().thread(tid))
            .ok_or_else(|| backend_error(LinuxError::UnknownCoreThread(pid.as_raw())))
    }
}

impl InspectionOps for CoreTarget {
    fn read_word(&self, _pid: Pid, address: u64) -> Result<u64> {
        let mut bytes = [0; 8];
        match self.memory.read(address, &mut bytes) {
            Ok(()) => Ok(u64::from_le_bytes(bytes)),
            Err(CoreMemoryError::Unavailable) => {
                Err(backend_error(LinuxError::MemoryInaccessible {
                    address: VirtualAddress::new(address),
                }))
            }
            Err(CoreMemoryError::Io(error)) => Err(error.into()),
        }
    }

    fn read_memory_word(
        &self,
        _pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        let mut bytes = [0; 8];
        match self.memory.read(address, &mut bytes) {
            Ok(()) => Ok(u64::from_le_bytes(bytes)),
            // Saved and file-backed memory need not end on a word boundary,
            // so find how much of the word is readable.
            Err(CoreMemoryError::Unavailable) => {
                let mut readable = 0;
                while readable < bytes.len() {
                    let Some(current) = address.checked_add(readable as u64) else {
                        break;
                    };
                    match self.memory.read(current, &mut bytes[readable..=readable]) {
                        Ok(()) => readable += 1,
                        Err(CoreMemoryError::Unavailable) => break,
                        Err(CoreMemoryError::Io(error)) => {
                            return Err(MemoryAccessError::Fatal(error.into()));
                        }
                    }
                }
                bytes[readable..].fill(0);
                if readable == 0 {
                    Err(MemoryAccessError::Inaccessible)
                } else {
                    Err(MemoryAccessError::Partial {
                        word: u64::from_le_bytes(bytes),
                        readable,
                    })
                }
            }
            Err(CoreMemoryError::Io(error)) => Err(MemoryAccessError::Fatal(error.into())),
        }
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        Ok(self.thread(pid)?.registers)
    }

    fn floating_registers(&self, pid: Pid) -> Result<Fxsave> {
        self.thread(pid)?
            .fxsave
            .clone()
            .ok_or_else(|| backend_error(LinuxError::CoreFloatingRegistersUnsaved(pid.as_raw())))
    }

    fn tls_address(
        &self,
        thread: Pid,
        link_map: VirtualAddress,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        let process = i32::try_from(self.memory.core().process.pid)
            .map_err(|_| Arc::from("the core dump's process identifier is invalid"))?;
        thread_db::tls_address(self, Pid::from_raw(process), thread, link_map, offset)
    }
}

/// An opened core dump and the controller thread serving its requests.
pub struct PostMortemSession {
    pub image: Arc<ModuleImage>,
    pub info: Arc<CoreDumpInfo>,
    pub controller: JoinHandle<()>,
}

impl From<CoreError> for Error {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Io(error) => Self::Io(error),
            CoreError::Invalid(message) => Self::InvalidCoreDump(message),
        }
    }
}

/// A module file placed at one recorded image.
struct PlacedFile {
    path: PathBuf,
    data: Arc<[u8]>,
    inode: u64,
    load_bias: u64,
    /// Dump-time addresses whose file bytes are the process's memory.
    read_only: Vec<Range<u64>>,
}

/// The module file chosen for one recorded image.
struct ImageFile {
    path: PathBuf,
    data: Arc<[u8]>,
    inode: u64,
    load_bias: u64,
    identity: ModuleIdentity,
    read_only: Vec<Range<u64>>,
}

impl PlacedFile {
    fn with_identity(self, identity: ModuleIdentity) -> ImageFile {
        ImageFile {
            path: self.path,
            data: self.data,
            inode: self.inode,
            load_bias: self.load_bias,
            identity,
            read_only: self.read_only,
        }
    }
}

/// Why a candidate file is not proven to be a recorded image, from least to
/// most usable.
#[derive(Debug)]
enum Rejection<T> {
    /// No load bias places the file at the image, which proves it differs.
    Unplaced(String),
    /// The file is placed but provably differs from the image.
    Mismatch(T, String),
    /// The dump saved nothing that confirms or refutes the file.
    Unverifiable(T),
}

impl<T> Rejection<T> {
    const fn usability(&self) -> u8 {
        match self {
            Self::Unplaced(_) => 0,
            Self::Mismatch(..) => 1,
            Self::Unverifiable(_) => 2,
        }
    }
}

#[derive(Debug)]
struct Rejected<T> {
    path: PathBuf,
    rejection: Rejection<T>,
}

/// Applies the identity policy once no candidate was proven. When mismatches
/// are allowed, the most usable placed candidate is used, the earliest among
/// equals; otherwise the most usable one explains the failure. No candidates
/// at all means the image's file is missing.
fn choose_unproven<T>(
    rejected: Vec<Rejected<T>>,
    allow: bool,
) -> Result<Option<(T, ModuleIdentity)>> {
    // `max_by_key` keeps the last maximum, so reversing keeps the earliest.
    let Some(best) = rejected
        .into_iter()
        .rev()
        .max_by_key(|candidate| candidate.rejection.usability())
    else {
        return Ok(None);
    };
    let path = best.path;
    match (best.rejection, allow) {
        (Rejection::Unverifiable(file), true) => Ok(Some((file, ModuleIdentity::Unverified))),
        (Rejection::Mismatch(file, detail), true) => Ok(Some((
            file,
            ModuleIdentity::Mismatched {
                detail: detail.into(),
            },
        ))),
        (Rejection::Unverifiable(_), false) => Err(Error::CoreModuleUnverified { path }),
        (Rejection::Mismatch(_, detail) | Rejection::Unplaced(detail), false) => {
            Err(Error::CoreModuleMismatch { path, detail })
        }
        // Allowing a mismatch permits using a file's metadata, not
        // relocating it to a guessed address.
        (Rejection::Unplaced(detail), true) => Err(Error::CoreModuleUnplaceable { path, detail }),
    }
}

/// What one candidate file proves about a recorded image.
enum Examined {
    Proven(ImageFile),
    Rejected(Rejected<PlacedFile>),
}

fn examine(
    core: &CoreDump,
    image: &ImageMappings,
    file: ModuleFile,
    entry: Option<u64>,
) -> Result<Examined> {
    let ModuleFile {
        path, data, inode, ..
    } = file;
    let data: Arc<[u8]> = data.into();
    let (load_bias, mut evidence, read_only) = match verify_image(core, image, &data)? {
        ImageVerification::Placed {
            load_bias,
            evidence,
            read_only,
        } => (load_bias, evidence, read_only),
        ImageVerification::Unplaced(detail) => {
            return Ok(Examined::Rejected(Rejected {
                path,
                rejection: Rejection::Unplaced(detail),
            }));
        }
    };
    // The kernel records where execution began; a different entry point is
    // proof of a different executable even when no other evidence survives.
    if let Some(recorded) = entry
        && !matches!(evidence, ImageEvidence::Mismatch(_))
    {
        // Verification already proved the file parses as an ELF image.
        let declared = object::File::parse(data.as_ref()).map_or(0, |object| object.entry());
        if load_bias.checked_add(declared) != Some(recorded) {
            evidence = ImageEvidence::Mismatch(format!(
                "the recorded entry point {recorded:#x} differs from the file's"
            ));
        }
    }
    let placed = PlacedFile {
        path: path.clone(),
        data,
        inode,
        load_bias,
        read_only,
    };
    let rejected = |rejection| Ok(Examined::Rejected(Rejected { path, rejection }));
    match evidence {
        ImageEvidence::BuildId => Ok(Examined::Proven(
            placed.with_identity(ModuleIdentity::BuildId),
        )),
        ImageEvidence::SavedContent { compared } => Ok(Examined::Proven(placed.with_identity(
            ModuleIdentity::SavedContent {
                compared_bytes: compared,
            },
        ))),
        ImageEvidence::Mismatch(detail) => rejected(Rejection::Mismatch(placed, detail)),
        ImageEvidence::Unverifiable => rejected(Rejection::Unverifiable(placed)),
    }
}

/// Finds the file for one recorded image: the first candidate proven to be
/// it, or else whichever unproven candidate the identity policy accepts.
/// Without a saved header only ELF candidates count, since a mapped data
/// file is no module.
///
/// When no candidate is proven, a different file at the recorded path is an
/// error unless mismatches are allowed, since it means this machine or
/// sysroot holds another build. A file that searching found merely shares a
/// name, so unless mismatches are allowed it counts only once proven.
fn resolve_image(
    core: &CoreDump,
    image: &ImageMappings,
    candidates: impl Iterator<Item = Result<ModuleFile>>,
    elf_only: bool,
    entry: Option<u64>,
    allow: bool,
) -> Result<Option<ImageFile>> {
    let mut examined = BTreeSet::new();
    let mut rejected = Vec::new();
    for candidate in candidates {
        let candidate = candidate?;
        // One file can be reached by name and by build-id.
        if (elf_only && !is_elf(&candidate.data)) || !examined.insert(candidate.path.clone()) {
            continue;
        }
        let searched = candidate.searched;
        match examine(core, image, candidate, entry)? {
            Examined::Proven(file) => return Ok(Some(file)),
            Examined::Rejected(candidate) if allow || !searched => rejected.push(candidate),
            Examined::Rejected(_) => {}
        }
    }
    Ok(choose_unproven(rejected, allow)?.map(|(file, identity)| file.with_identity(identity)))
}

/// Every candidate for a recorded image, searching by build-id only once
/// every file found by name has been examined.
fn candidates<'a>(
    locator: &'a ModuleLocator,
    recorded: &'a Path,
    build_id: Option<&'a [u8]>,
) -> impl Iterator<Item = Result<ModuleFile>> + 'a {
    locator.named(recorded).chain(
        build_id
            .into_iter()
            .flat_map(|build_id| locator.with_build_id(build_id)),
    )
}

/// Loaded state for every recorded image other than the main executable.
struct LibraryImage {
    image: ImageMappings,
    file: ImageFile,
    loaded: LoadedModule,
    debug: crate::debug_info::DebugInfo,
}

/// Every recorded image resolved to a file, or explicitly missing.
struct ResolvedModules {
    main_image: ImageMappings,
    main_file: ImageFile,
    main_debug: crate::debug_info::DebugInfo,
    libraries: Vec<LibraryImage>,
    modules: Vec<CoreModule>,
}

impl ResolvedModules {
    /// Only proven files may stand in for memory the dump did not save.
    fn backings(&self) -> Vec<FileBacking> {
        let mut backings = Vec::new();
        if self.main_file.identity.is_verified() {
            backings.extend(FileBacking::for_image(
                &self.main_image,
                &self.main_file.data,
                &self.main_file.read_only,
            ));
        }
        for library in self
            .libraries
            .iter()
            .filter(|library| library.file.identity.is_verified())
        {
            backings.extend(FileBacking::for_image(
                &library.image,
                &library.file.data,
                &library.file.read_only,
            ));
        }
        backings
    }
}

fn core_module(
    image: &ImageMappings,
    build_id: Option<Vec<u8>>,
    state: CoreModuleState,
) -> CoreModule {
    CoreModule {
        recorded_path: Arc::new(image.path.clone()),
        start: VirtualAddress::new(image.start()),
        build_id: build_id.map(Into::into),
        state,
    }
}

fn loaded_state(loaded: LoadedModule, file: &ImageFile) -> CoreModuleState {
    CoreModuleState::Loaded {
        module: LoadedModuleRecord {
            module: loaded,
            path: Arc::new(file.path.clone()),
        },
        identity: file.identity.clone(),
    }
}

/// The executable's recorded image, its recorded build-id, and its file.
struct MainImage {
    image: ImageMappings,
    build_id: Option<Vec<u8>>,
    file: ImageFile,
}

/// Selects the executable's image through the program headers recorded in
/// the auxiliary vector, then finds its file unless one was supplied.
fn resolve_executable(
    core: &CoreDump,
    images: &[ImageMappings],
    locator: &ModuleLocator,
    options: &CoreDumpOptions,
) -> Result<MainImage> {
    let program_headers = core.auxv_value(AT_PHDR).ok_or_else(|| {
        Error::CoreExecutableUnavailable(
            "the dump records no program-header address in its auxiliary vector".to_owned(),
        )
    })?;
    let image = images
        .iter()
        .find(|image| image.contains(program_headers))
        .ok_or_else(|| {
            Error::CoreExecutableUnavailable(
                "no recorded file mapping contains the program headers".to_owned(),
            )
        })?
        .clone();
    let build_id = recorded_build_id(core, &image)?;
    let entry = core.auxv_value(AT_ENTRY);
    let allow = options.allow_module_mismatch;
    let file = match &options.executable {
        Some(path) => resolve_image(
            core,
            &image,
            std::iter::once(open_explicit(path)),
            false,
            entry,
            allow,
        )?,
        None => resolve_image(
            core,
            &image,
            candidates(locator, &image.path, build_id.as_deref()),
            false,
            entry,
            allow,
        )?,
    }
    .ok_or_else(|| {
        let recorded = build_id
            .as_deref()
            .map(|build_id| format!(" (build-id {})", hex(build_id)))
            .unwrap_or_default();
        Error::CoreExecutableUnavailable(format!(
            "{}{recorded} {}; supply the executable explicitly",
            image.path.display(),
            locator.absence()
        ))
    })?;
    Ok(MainImage {
        image,
        build_id,
        file,
    })
}

/// Finds, verifies, and loads every recorded image, beginning with the
/// executable.
fn resolve_modules(core: &CoreDump, options: &CoreDumpOptions) -> Result<ResolvedModules> {
    let locator = ModuleLocator::new(options.sysroot.as_deref(), &options.module_paths)?;
    let images = image_mappings(&core.files);
    let MainImage {
        image: main_image,
        build_id: main_build_id,
        file: main_file,
    } = resolve_executable(core, &images, &locator, options)?;
    let main_debug = crate::debug_info::load_bytes(&main_file.path, &main_file.data)?;
    let main_loaded = LoadedModule::main(main_debug.image.id(), main_file.load_bias);

    let mut modules = vec![core_module(
        &main_image,
        main_build_id,
        loaded_state(main_loaded, &main_file),
    )];
    let mut libraries = Vec::new();
    for image in &images {
        if *image == main_image {
            continue;
        }
        // Without a saved header, only an ELF file can say that the image
        // is a module, and finding none leaves nothing to report.
        let elf_only = match saved_header(core, image)? {
            SavedHeader::Elf => false,
            SavedHeader::Data => continue,
            SavedHeader::Unsaved => true,
        };
        let build_id = recorded_build_id(core, image)?;
        let found = resolve_image(
            core,
            image,
            candidates(&locator, &image.path, build_id.as_deref()),
            elf_only,
            None,
            options.allow_module_mismatch,
        )?;
        let Some(file) = found else {
            if !elf_only {
                modules.push(core_module(image, build_id, CoreModuleState::Missing));
            }
            continue;
        };
        let number = u32::try_from(libraries.len() + 1)
            .map_err(|_| backend_error(LinuxError::ModuleIdExhausted))?;
        let image_id = ModuleImageId::new(number);
        let debug = crate::debug_info::load_module_bytes(&file.path, &file.data, image_id)?;
        let loaded = LoadedModule {
            id: ModuleId::new(number),
            image: image_id,
            load_bias: file.load_bias,
        };
        modules.push(core_module(image, build_id, loaded_state(loaded, &file)));
        libraries.push(LibraryImage {
            image: image.clone(),
            file,
            loaded,
            debug,
        });
    }
    Ok(ResolvedModules {
        main_image,
        main_file,
        main_debug,
        libraries,
        modules,
    })
}

/// Opens a core dump, verifies and loads its modules, and starts a controller
/// that serves read-only requests against its single stopped snapshot.
pub fn open_core(
    options: &CoreDumpOptions,
    channels: ControllerChannels,
) -> Result<PostMortemSession> {
    let core = CoreDump::open(&options.core)?;
    let core_path = options.core.canonicalize()?;
    let resolved = resolve_modules(&core, options)?;
    let backings = resolved.backings();
    let resolved_main_path = resolved.main_image.path.clone();
    let ResolvedModules {
        main_file,
        main_debug,
        libraries,
        modules,
        ..
    } = resolved;

    let process = core.process.clone();
    let exception = core_exception(&core.threads[0]);
    let info = Arc::new(CoreDumpInfo {
        path: Arc::new(core_path),
        process_id: ProcessId::new(u64::from(process.pid)),
        process_name: process.name.clone(),
        arguments: process.arguments.clone(),
        exception: exception.clone(),
        modules: modules.into(),
    });
    let symbol_modules = std::iter::once(SymbolModule {
        recorded_path: resolved_main_path,
        load_bias: main_file.load_bias,
        image: Arc::clone(&main_debug.image),
    })
    .chain(libraries.iter().map(|library| SymbolModule {
        recorded_path: library.image.path.clone(),
        load_bias: library.loaded.load_bias,
        image: Arc::clone(&library.debug.image),
    }))
    .collect();
    let target = CoreTarget {
        memory: CoreMemory::new(Arc::new(core), backings),
        symbol_modules,
    };
    let image = Arc::clone(&main_debug.image);
    let main_loaded = LoadedModule::main(image.id(), main_file.load_bias);
    let executable = ExecutableSource {
        display_path: Arc::new(main_file.path.clone()),
        identity: FileIdentity {
            inode: main_file.inode,
        },
        data: main_file.data,
        process_start_time: None,
    };
    let (ready_sender, ready) = std::sync::mpsc::sync_channel(1);
    let controller = thread::Builder::new()
        .name(POST_MORTEM_THREAD_NAME.into())
        .spawn(move || {
            let mut controller = Controller::new(
                SessionLease::detached(),
                executable,
                main_debug,
                channels,
                target,
            );
            let initialized = controller.initialize_post_mortem(
                process.pid,
                main_loaded,
                libraries,
                &StopReason::CoreDump { exception },
            );
            let failed = initialized.is_err();
            let _ = ready_sender.send(initialized);
            if !failed {
                controller.run_post_mortem();
            }
        })?;
    match ready.recv() {
        Ok(Ok(())) => Ok(PostMortemSession {
            image,
            info,
            controller,
        }),
        Ok(Err(error)) => {
            let _ = controller.join();
            Err(error)
        }
        Err(_) => {
            let _ = controller.join();
            Err(Error::BackendThreadPanicked)
        }
    }
}

impl Controller<CoreTarget> {
    fn initialize_post_mortem(
        &mut self,
        process: u32,
        main: LoadedModule,
        libraries: Vec<LibraryImage>,
        reason: &StopReason,
    ) -> Result<()> {
        let tgid = Pid::from_raw(
            i32::try_from(process).map_err(|_| Error::InvalidProcessId(u64::from(process)))?,
        );
        let threads = self
            .ptrace
            .memory
            .core()
            .threads
            .iter()
            .map(|thread| {
                i32::try_from(thread.tid).map(Pid::from_raw).map_err(|_| {
                    Error::InvalidCoreDump(format!("thread {} is invalid", thread.tid))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let selected = threads[0];
        // Loader link maps locate each module's TLS block. A dump that lost the
        // loader's state leaves TLS explicitly unavailable instead of failing.
        let link_maps = loader_link_maps(&self.ptrace, selected, &self.executable_data, main)
            .unwrap_or_default();

        let main_module = self
            .modules
            .get_mut(&main.id)
            .expect("main module is registered");
        main_module.loaded = main;
        main_module.link_map = link_maps.get(&main.load_bias).copied();
        for library in libraries {
            self.modules.insert(
                library.loaded.id,
                RuntimeModule {
                    loaded: library.loaded,
                    image: library.debug.image,
                    unwind: library.debug.unwind,
                    variables: library.debug.variables,
                    link_map: link_maps.get(&library.loaded.load_bias).copied(),
                },
            );
        }
        self.next_module_id = u32::try_from(self.modules.len())
            .map_err(|_| backend_error(LinuxError::ModuleIdExhausted))?;
        self.next_image_id = self.next_module_id;

        let trace_threads = threads
            .iter()
            .map(|&pid| {
                let mut thread = TraceThread::starting(ExpectedStop::None);
                thread.state = NativeThreadState::Stopped;
                if pid == selected {
                    thread.reason = Some(reason.clone());
                }
                (pid, thread)
            })
            .collect();
        self.inferior = Some(Inferior {
            public_stop: Some(PublicStop {
                id: allocate_stop_id(),
                triggering_thread: selected,
                reason: reason.clone(),
                presentations: BTreeMap::new(),
                selected_frames: BTreeMap::new(),
            }),
            selected_thread: Some(selected),
            ..Inferior::new(InferiorOrigin::PostMortem, tgid, main, trace_threads, None)
        });
        let presentation = self.presentation_for_thread(selected, Some(reason))?;
        self.inferior
            .as_mut()
            .and_then(|inferior| inferior.public_stop.as_mut())
            .expect("post-mortem stop exists")
            .presentations
            .insert(selected, presentation);
        self.bump_revision();
        Ok(())
    }

    fn run_post_mortem(mut self) {
        while let Some(message) = self.messages.blocking_recv() {
            let ControllerMessage::Request(request) = message else {
                continue;
            };
            match request {
                Request::Shutdown { reply } => {
                    let _ = reply.send(Ok(()));
                    return;
                }
                Request::AddBreakpoint { reply, .. }
                | Request::RemoveBreakpoint { reply, .. }
                | Request::SetBreakpointHitCondition { reply, .. } => reject(reply),
                Request::RemoveAllBreakpoints { reply } => reject(reply),
                Request::ResolveWatchTarget { reply, .. } => reject(reply),
                Request::AddWatchpoint { reply, .. } | Request::RemoveWatchpoint { reply, .. } => {
                    reject(reply);
                }
                Request::RemoveAllWatchpoints { reply } => reject(reply),
                Request::Launch { reply, .. }
                | Request::Continue { reply, .. }
                | Request::Step { reply, .. }
                | Request::Pause { reply, .. } => reject(reply),
                Request::Attach { reply, .. } => reject(reply),
                Request::WriteWord { reply, .. } => reject(reply),
                request => self.handle_inspection_request(request),
            }
        }
    }
}

fn reject<T>(reply: Reply<T>) {
    let _ = reply.send(Err(Error::PostMortemTarget));
}

/// Describes the signal recorded for the thread that triggered the dump.
fn core_exception(thread: &CoreThread) -> Option<ExceptionInfo> {
    let signal = thread.signal.unwrap_or_else(|| CoreSignal {
        number: i32::from(thread.current_signal),
        code: 0,
        fault_address: None,
        sender: None,
    });
    let number = u64::try_from(signal.number)
        .ok()
        .filter(|number| *number != 0)?;
    let name = NixSignal::try_from(signal.number).map_or_else(
        |_| format!("signal {}", signal.number),
        |signal| signal.to_string(),
    );
    if thread.signal.is_none() {
        return Some(ExceptionInfo::new(number, name));
    }
    let code = signal_code_name(signal.number, signal.code)
        .map_or_else(|| format!("si_code {}", signal.code), str::to_owned);
    let detail = match (signal.fault_address, signal.sender) {
        (Some(address), _) => format!("{name} ({code}) at {address:#x}"),
        (None, Some(sender)) if sender > 0 => format!("{name} ({code}) sent by process {sender}"),
        _ => format!("{name} ({code})"),
    };
    Some(ExceptionInfo::new(number, detail))
}

fn signal_code_name(signal: i32, code: i32) -> Option<&'static str> {
    let generic = match code {
        0 => Some("SI_USER"),
        0x80 => Some("SI_KERNEL"),
        -1 => Some("SI_QUEUE"),
        -2 => Some("SI_TIMER"),
        -3 => Some("SI_MESGQ"),
        -4 => Some("SI_ASYNCIO"),
        -5 => Some("SI_SIGIO"),
        -6 => Some("SI_TKILL"),
        _ => None,
    };
    if generic.is_some() {
        return generic;
    }
    let names: &[&str] = match signal {
        libc::SIGSEGV => &["SEGV_MAPERR", "SEGV_ACCERR", "SEGV_BNDERR", "SEGV_PKUERR"],
        libc::SIGBUS => &[
            "BUS_ADRALN",
            "BUS_ADRERR",
            "BUS_OBJERR",
            "BUS_MCEERR_AR",
            "BUS_MCEERR_AO",
        ],
        libc::SIGILL => &[
            "ILL_ILLOPC",
            "ILL_ILLOPN",
            "ILL_ILLADR",
            "ILL_ILLTRP",
            "ILL_PRVOPC",
            "ILL_PRVREG",
            "ILL_COPROC",
            "ILL_BADSTK",
        ],
        libc::SIGFPE => &[
            "FPE_INTDIV",
            "FPE_INTOVF",
            "FPE_FLTDIV",
            "FPE_FLTOVF",
            "FPE_FLTUND",
            "FPE_FLTRES",
            "FPE_FLTINV",
            "FPE_FLTSUB",
        ],
        libc::SIGTRAP => &["TRAP_BRKPT", "TRAP_TRACE", "TRAP_BRANCH", "TRAP_HWBKPT"],
        _ => &[],
    };
    usize::try_from(code)
        .ok()
        .and_then(|code| code.checked_sub(1))
        .and_then(|index| names.get(index).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unproven_candidates_follow_the_identity_policy_in_order_of_usability() {
        let candidate = |path: &str, rejection| Rejected {
            path: PathBuf::from(path),
            rejection,
        };
        let unplaced = || Rejection::Unplaced("unplaced".to_owned());
        let mismatch = |file| Rejection::Mismatch(file, format!("{file} differs"));
        let mixed = || {
            vec![
                candidate("/a", unplaced()),
                candidate("/b", mismatch("b")),
                candidate("/c", Rejection::Unverifiable("c")),
                candidate("/d", Rejection::Unverifiable("d")),
            ]
        };
        let mismatched = || {
            vec![
                candidate("/a", unplaced()),
                candidate("/b", mismatch("b")),
                candidate("/c", mismatch("c")),
            ]
        };
        let only_unplaced = || vec![candidate("/a", unplaced())];

        // Nothing found is a missing file in either mode.
        assert!(matches!(choose_unproven::<()>(Vec::new(), false), Ok(None)));
        assert!(matches!(choose_unproven::<()>(Vec::new(), true), Ok(None)));

        // Allowed: a file that may be right beats one known to be wrong,
        // and the earliest wins among equals.
        assert!(matches!(
            choose_unproven(mixed(), true),
            Ok(Some(("c", ModuleIdentity::Unverified)))
        ));
        assert!(matches!(
            choose_unproven(mismatched(), true),
            Ok(Some(("b", ModuleIdentity::Mismatched { detail }))) if &*detail == "b differs"
        ));
        assert!(matches!(
            choose_unproven(only_unplaced(), true),
            Err(Error::CoreModuleUnplaceable { path, .. }) if path == Path::new("/a")
        ));

        // Strict: the most usable candidate explains the failure.
        assert!(matches!(
            choose_unproven(mixed(), false),
            Err(Error::CoreModuleUnverified { path }) if path == Path::new("/c")
        ));
        assert!(matches!(
            choose_unproven(mismatched(), false),
            Err(Error::CoreModuleMismatch { path, detail })
                if path == Path::new("/b") && detail == "b differs"
        ));
        assert!(matches!(
            choose_unproven(only_unplaced(), false),
            Err(Error::CoreModuleMismatch { path, detail })
                if path == Path::new("/a") && detail == "unplaced"
        ));
    }
}
