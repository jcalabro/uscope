//! Thread-local storage addresses, found the way the inferior's C library
//! lays its threads out.
//!
//! Debug information locates a TLS variable by its offset in its module's
//! TLS block, of which each thread has its own copy. Which copy a thread
//! uses is recorded in C library structures. glibc is asked through its
//! `libthread_db` ([`thread_db`]) or, for a version that library refuses,
//! through glibc's own layout descriptors ([`glibc`]). musl has no thread
//! debugging library, and the structures it is read through are fixed ABI
//! ([`musl`]).

use std::collections::BTreeMap;
use std::io::IoSliceMut;
use std::num::NonZeroU64;
use std::sync::Arc;

use nix::libc;
use nix::sys::ptrace;
use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use object::read::elf::ProgramHeader as _;
use object::{Object, ObjectSymbol, SymbolKind};

use super::{mapped_module_load_bias, module_mappings};
use crate::VirtualAddress;

mod glibc;
mod musl;
mod thread_db;

/// The prefix of the file name of musl's dynamic loader, which is also its
/// C library: `ld-musl-x86_64.so.1` on x86-64.
const MUSL_LOADER_PREFIX: &[u8] = b"ld-musl-";
/// The musl function that lays out a new thread's TLS, which a statically
/// linked musl program always contains.
const MUSL_COPY_TLS: &str = "__copy_tls";

/// Makes every glibc TLS lookup in this process use glibc's layout
/// descriptors rather than `libthread_db`, so tests can check that the two
/// agree.
pub(super) fn force_glibc_descriptors(forced: bool) {
    glibc::force(forced);
}

/// The C library whose structures locate a program's TLS blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CLibrary {
    /// glibc, or any C library not recognized as musl, whose lookups then
    /// fail with `libthread_db`'s reason.
    Glibc,
    Musl,
}

impl CLibrary {
    /// Recognizes musl from the executable: a dynamically linked program
    /// names musl's loader as its interpreter, and a statically linked one
    /// contains musl's TLS layout function.
    pub(super) fn of_executable(data: &[u8]) -> Self {
        let Ok(object) = object::File::parse(data) else {
            return Self::Glibc;
        };
        let interpreter = match &object {
            object::File::Elf64(file) => {
                let endian = file.endian();
                file.elf_program_headers()
                    .iter()
                    .find_map(|segment| segment.interpreter(endian, data).ok().flatten())
            }
            _ => return Self::Glibc,
        };
        let musl = interpreter.map_or_else(
            || object.symbol_by_name(MUSL_COPY_TLS).is_some(),
            |path| {
                path.rsplit(|&byte| byte == b'/')
                    .next()
                    .is_some_and(|name| name.starts_with(MUSL_LOADER_PREFIX))
            },
        );
        if musl { Self::Musl } else { Self::Glibc }
    }

    /// Names each module's TLS block, by load bias, as this C library does.
    /// `link_maps` are the loader's modules in its list order, with their
    /// load biases; `has_tls` says which loaded modules have TLS, and lacks
    /// those the debugger could not load.
    pub(super) fn tls_modules(
        self,
        link_maps: &[(u64, VirtualAddress)],
        main_load_bias: u64,
        has_tls: &BTreeMap<u64, bool>,
    ) -> BTreeMap<u64, TlsModule> {
        match self {
            Self::Glibc => link_maps
                .iter()
                .map(|&(load_bias, link_map)| (load_bias, TlsModule::Glibc { link_map }))
                .collect(),
            Self::Musl => {
                // musl numbers the modules with TLS from one, in load order,
                // and never unloads one. The executable heads the list, so it
                // is first even before the loader publishes the list, and is
                // the only module of a statically linked program.
                let order = if link_maps.is_empty() {
                    vec![main_load_bias]
                } else {
                    link_maps.iter().map(|&(load_bias, _)| load_bias).collect()
                };
                let mut modules = BTreeMap::new();
                let mut id = 0;
                for load_bias in order {
                    // Whether a module the debugger could not load has TLS is
                    // unknown, and so is every later module's number.
                    match has_tls.get(&load_bias) {
                        None => break,
                        Some(false) => {}
                        Some(true) => {
                            id += 1;
                            let id = NonZeroU64::new(id).expect("module numbers start at one");
                            modules.insert(load_bias, TlsModule::Musl { id });
                        }
                    }
                }
                modules
            }
        }
    }
}

/// How the C library identifies one module's TLS block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TlsModule {
    /// glibc finds a block through the module's loader `link_map`.
    Glibc { link_map: VirtualAddress },
    /// musl indexes each thread's dynamic thread vector by module number.
    Musl { id: NonZeroU64 },
}

/// The address of `offset` within `module`'s TLS block for `thread`.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    process: Pid,
    thread: Pid,
    module: TlsModule,
    offset: u64,
) -> Result<VirtualAddress, Arc<str>> {
    match module {
        TlsModule::Glibc { link_map } => {
            thread_db::tls_address(services, process, thread, link_map, offset)
        }
        TlsModule::Musl { id } => musl::tls_address(services, thread, id, offset)
            .map(VirtualAddress::new)
            .map_err(|error| error.to_string().into()),
    }
}

/// Read-only process state that TLS lookups query.
pub(super) trait ProcessServices {
    fn read(&self, address: u64, output: &mut [u8]) -> bool;
    fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct>;
    fn lookup_symbol(&self, object: &str, symbol: &str) -> Option<u64>;
}

/// A live process read through `process_vm_readv`, ptrace, and `/proc`.
pub(super) struct LiveProcess {
    pub(super) pid: Pid,
}

impl ProcessServices for LiveProcess {
    fn read(&self, address: u64, output: &mut [u8]) -> bool {
        usize::try_from(address).is_ok_and(|address| read_process(self.pid, address, output))
    }

    fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct> {
        ptrace::getregs(lwp).ok()
    }

    fn lookup_symbol(&self, object: &str, symbol: &str) -> Option<u64> {
        lookup_symbol(self.pid, object, symbol)
    }
}

fn read_process(pid: Pid, address: usize, output: &mut [u8]) -> bool {
    let size = output.len();
    let mut local = [IoSliceMut::new(output)];
    let remote = [RemoteIoVec {
        base: address,
        len: size,
    }];
    process_vm_readv(pid, &mut local, &remote).is_ok_and(|read| read == size)
}

fn lookup_symbol(pid: Pid, requested_object: &str, requested_symbol: &str) -> Option<u64> {
    let mappings = module_mappings(pid).ok()?;
    let mut fallback = None;
    for mapping in mappings {
        let Some(file_name) = mapping.path.file_name() else {
            continue;
        };
        let file_name = file_name.to_string_lossy();
        let preferred = file_name.starts_with(requested_object);
        let Ok(bias) = mapped_module_load_bias(&mapping) else {
            continue;
        };
        let Ok(data) = std::fs::read(&mapping.path) else {
            continue;
        };
        let Ok(object) = object::File::parse(data.as_slice()) else {
            continue;
        };
        // Undefined symbols have no address here, and a TLS symbol's value
        // is an offset rather than an address.
        let Some(symbol) = object
            .dynamic_symbols()
            .chain(object.symbols())
            .find(|symbol| {
                symbol.is_definition()
                    && symbol.kind() != SymbolKind::Tls
                    && symbol.name().ok() == Some(requested_symbol)
            })
        else {
            continue;
        };
        let Some(address) = bias.checked_add(symbol.address()) else {
            continue;
        };
        if preferred {
            return Some(address);
        }
        fallback.get_or_insert(address);
    }
    fallback
}

/// Why a TLS address could not be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TlsError {
    /// The thread has not allocated the module's block, so it cannot have
    /// observed any value in it.
    Deferred,
    /// The module has no TLS block.
    NoTls,
    Failed(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deferred => {
                formatter.write_str("the thread has not allocated the module's TLS block")
            }
            Self::NoTls => formatter.write_str("the module has no TLS block"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

fn failed(message: impl Into<String>) -> TlsError {
    TlsError::Failed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn musl(id: u64) -> TlsModule {
        TlsModule::Musl {
            id: NonZeroU64::new(id).expect("module numbers start at one"),
        }
    }

    #[test]
    fn musl_numbers_the_modules_with_tls_in_load_order() {
        let link_maps = [0x1000, 0x2000, 0x3000, 0x4000, 0x5000]
            .map(|load_bias| (load_bias, VirtualAddress::new(load_bias + 1)));
        // The executable and a library have TLS and the loader none; the
        // fourth module could not be loaded.
        let has_tls = BTreeMap::from([
            (0x1000, true),
            (0x2000, false),
            (0x3000, true),
            (0x5000, true),
        ]);
        assert_eq!(
            CLibrary::Musl.tls_modules(&link_maps, 0x1000, &has_tls),
            BTreeMap::from([(0x1000, musl(1)), (0x3000, musl(2))])
        );
        // Without the loader's list, only the executable has a number.
        assert_eq!(
            CLibrary::Musl.tls_modules(&[], 0x1000, &has_tls),
            BTreeMap::from([(0x1000, musl(1))])
        );
        assert_eq!(
            CLibrary::Musl.tls_modules(&link_maps[1..3], 0x2000, &has_tls),
            BTreeMap::from([(0x3000, musl(1))])
        );
        // glibc finds every listed module's block through its link map.
        assert_eq!(
            CLibrary::Glibc.tls_modules(&link_maps[..2], 0x1000, &has_tls),
            BTreeMap::from([
                (
                    0x1000,
                    TlsModule::Glibc {
                        link_map: VirtualAddress::new(0x1001)
                    }
                ),
                (
                    0x2000,
                    TlsModule::Glibc {
                        link_map: VirtualAddress::new(0x2001)
                    }
                ),
            ])
        );
    }
}
