//! Post-mortem sessions present a core dump as one permanent all-stop.
//!
//! Every inspection request shares the live controller's implementation over
//! [`CoreTarget`]. Requests that would execute, modify, or trap the target are
//! rejected before reaching any process-control state.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::ops::Range;
use std::os::unix::fs::MetadataExt as _;
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
    is_elf, saved_header, verify_image,
};
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

/// A module file read for one recorded image.
struct ImageFile {
    path: PathBuf,
    data: Arc<[u8]>,
    load_bias: u64,
    identity: ModuleIdentity,
    /// Dump-time addresses whose file bytes are the process's memory.
    read_only: Vec<Range<u64>>,
}

/// Applies the identity policy: proven files are always used; mismatched or
/// unverifiable files only when the caller explicitly allowed it.
fn accept_identity(path: &Path, evidence: ImageEvidence, allow: bool) -> Result<ModuleIdentity> {
    match evidence {
        ImageEvidence::BuildId => Ok(ModuleIdentity::BuildId),
        ImageEvidence::SavedContent { compared } => Ok(ModuleIdentity::SavedContent {
            compared_bytes: compared,
        }),
        ImageEvidence::Mismatch(detail) if allow => Ok(ModuleIdentity::Mismatched {
            detail: detail.into(),
        }),
        ImageEvidence::Mismatch(detail) => Err(Error::CoreModuleMismatch {
            path: path.to_owned(),
            detail,
        }),
        ImageEvidence::Unverifiable if allow => Ok(ModuleIdentity::Unverified),
        ImageEvidence::Unverifiable => Err(Error::CoreModuleUnverified {
            path: path.to_owned(),
        }),
    }
}

/// Reads a file the core dump names. Paths come from untrusted notes and can
/// name devices or FIFOs, such as `/dev/zero` for shared anonymous memory,
/// which would never finish reading; only regular files are opened.
fn read_regular_file(path: &Path) -> io::Result<Vec<u8>> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    fs::read(path)
}

/// Whether a regular file begins with the ELF magic number.
fn starts_like_elf(path: &Path) -> bool {
    use io::Read as _;

    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut magic = Vec::with_capacity(4);
    fs::File::open(path)
        .and_then(|file| file.take(4).read_to_end(&mut magic))
        .is_ok_and(|_| is_elf(&magic))
}

fn read_image_file(
    core: &CoreDump,
    image: &ImageMappings,
    path: &Path,
    allow: bool,
    entry: Option<u64>,
) -> Result<ImageFile> {
    let data: Arc<[u8]> = read_regular_file(path)?.into();
    let path = path.canonicalize()?;
    let (load_bias, mut evidence, read_only) = match verify_image(core, image, &data)? {
        ImageVerification::Placed {
            load_bias,
            evidence,
            read_only,
        } => (load_bias, evidence, read_only),
        // Allowing a mismatch permits using a file's metadata, not
        // relocating it to a guessed address.
        ImageVerification::Unplaced(detail) if allow => {
            return Err(Error::CoreModuleUnplaceable { path, detail });
        }
        ImageVerification::Unplaced(detail) => {
            return Err(Error::CoreModuleMismatch { path, detail });
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
    let identity = accept_identity(&path, evidence, allow)?;
    Ok(ImageFile {
        path,
        data,
        load_bias,
        identity,
        read_only,
    })
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

fn loaded_core_module(image: &ImageMappings, loaded: LoadedModule, file: &ImageFile) -> CoreModule {
    CoreModule {
        recorded_path: Arc::new(image.path.clone()),
        start: VirtualAddress::new(image.start()),
        state: CoreModuleState::Loaded {
            module: LoadedModuleRecord {
                module: loaded,
                path: Arc::new(file.path.clone()),
            },
            identity: file.identity.clone(),
        },
    }
}

/// Selects the executable through the program headers recorded in the
/// auxiliary vector, then verifies and loads every recorded image.
fn resolve_modules(core: &CoreDump, options: &CoreDumpOptions) -> Result<ResolvedModules> {
    let images = image_mappings(&core.files);
    let program_headers = core.auxv_value(AT_PHDR).ok_or_else(|| {
        Error::CoreExecutableUnavailable(
            "the dump records no program-header address in its auxiliary vector".to_owned(),
        )
    })?;
    let main_image = images
        .iter()
        .find(|image| image.contains(program_headers))
        .ok_or_else(|| {
            Error::CoreExecutableUnavailable(
                "no recorded file mapping contains the program headers".to_owned(),
            )
        })?
        .clone();
    let executable_path = options
        .executable
        .clone()
        .unwrap_or_else(|| main_image.path.clone());
    let main_file = read_image_file(
        core,
        &main_image,
        &executable_path,
        options.allow_module_mismatch,
        core.auxv_value(AT_ENTRY),
    )
    .map_err(|error| match error {
        Error::Io(error)
            if error.kind() == io::ErrorKind::NotFound && options.executable.is_none() =>
        {
            Error::CoreExecutableUnavailable(format!(
                "{} no longer exists; supply the executable explicitly",
                executable_path.display()
            ))
        }
        error => error,
    })?;
    let main_debug = crate::debug_info::load_bytes(&main_file.path, &main_file.data)?;
    let main_loaded = LoadedModule::main(main_debug.image.id(), main_file.load_bias);

    let mut modules = vec![loaded_core_module(&main_image, main_loaded, &main_file)];
    let mut libraries = Vec::new();
    for image in &images {
        if *image == main_image {
            continue;
        }
        // Without a saved header, a file that is an ELF image on disk is
        // treated as a module so that its verification is reported rather
        // than silently skipped.
        let header = saved_header(core, image)?;
        let candidate = match header {
            SavedHeader::Elf => true,
            SavedHeader::Data => false,
            SavedHeader::Unsaved => starts_like_elf(&image.path),
        };
        if !candidate {
            continue;
        }
        let file = match read_image_file(
            core,
            image,
            &image.path,
            options.allow_module_mismatch,
            None,
        ) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                modules.push(CoreModule {
                    recorded_path: Arc::new(image.path.clone()),
                    start: VirtualAddress::new(image.start()),
                    state: CoreModuleState::Missing,
                });
                continue;
            }
            Err(error) => return Err(error),
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
        modules.push(loaded_core_module(image, loaded, &file));
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
            inode: fs::metadata(&main_file.path)?.ino(),
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
                Request::AddBreakpoint { reply, .. } | Request::RemoveBreakpoint { reply, .. } => {
                    reject(reply);
                }
                Request::RemoveAllBreakpoints { reply } => reject(reply),
                Request::ResolveWatchTarget { reply, .. } => reject(reply),
                Request::AddWatchpoint { reply, .. } | Request::RemoveWatchpoint { reply, .. } => {
                    reject(reply);
                }
                Request::RemoveAllWatchpoints { reply } => reject(reply),
                Request::Launch { reply }
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
