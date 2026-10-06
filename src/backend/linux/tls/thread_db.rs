#![allow(unsafe_code)]

//! Narrow glibc `libthread_db` boundary for ABI-correct TLS lookup.
//!
//! This FFI boundary holds most of uscope's unsafe code. Its C
//! process-service callbacks are synchronous, read-only, and bounded. Each
//! agent carries the [`ProcessServices`] that answer them: a live process
//! owned by the ptrace controller, or a post-mortem core dump.
//!
//! `libthread_db` keeps a process-wide agent list without synchronization,
//! so every agent is created, used, and deleted under [`THREAD_DB`]. Sessions
//! run on separate controller threads and may look up TLS concurrently.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use nix::libc;
use nix::unistd::Pid;

use super::{ProcessServices, glibc};
use crate::VirtualAddress;

const TD_OK: c_int = 0;
const TD_ERR: c_int = 1;
const TD_NOLIBTHREAD: c_int = 12;
const TD_TLSDEFER: c_int = 21;
const TD_VERSION: c_int = 22;
/// `td_err_e` names, indexed by value.
const TD_ERRORS: [&str; 24] = [
    "TD_OK",
    "TD_ERR",
    "TD_NOTHR",
    "TD_NOSV",
    "TD_NOLWP",
    "TD_BADPH",
    "TD_BADTH",
    "TD_BADSH",
    "TD_BADTA",
    "TD_BADKEY",
    "TD_NOMSG",
    "TD_NOFPREGS",
    "TD_NOLIBTHREAD",
    "TD_NOEVENT",
    "TD_NOCAPAB",
    "TD_DBERR",
    "TD_NOAPLIC",
    "TD_NOTSD",
    "TD_MALLOC",
    "TD_PARTIALREG",
    "TD_NOXREGS",
    "TD_TLSDEFER",
    "TD_VERSION",
    "TD_NOTLS",
];
const PS_OK: c_int = 0;
const PS_ERR: c_int = 1;
const PS_BADLID: c_int = 3;
const PS_NOSYM: c_int = 5;
const X86_64_GREG_COUNT: usize = 27;
/// The `<sys/reg.h>` index of FS, the only thread area x86-64 glibc queries.
const X86_64_FS_INDEX: c_int = 25;

/// Serializes all use of `libthread_db`; see the module documentation.
static THREAD_DB: Mutex<()> = Mutex::new(());

/// The opaque `ps_prochandle` passed back to every callback.
struct ProcessHandle<'a> {
    pid: c_int,
    services: &'a dyn ProcessServices,
}

#[repr(C)]
struct ThreadAgent {
    _private: [u8; 0],
}

#[repr(C)]
struct ThreadHandle {
    agent: *mut ThreadAgent,
    unique: *mut c_void,
}

#[link(name = "thread_db")]
unsafe extern "C" {
    fn td_init() -> c_int;
    fn td_ta_new(process: *mut c_void, agent: *mut *mut ThreadAgent) -> c_int;
    fn td_ta_delete(agent: *mut ThreadAgent) -> c_int;
    fn td_ta_map_lwp2thr(agent: *const ThreadAgent, lwp: c_int, handle: *mut ThreadHandle)
    -> c_int;
    fn td_thr_tls_get_addr(
        handle: *const ThreadHandle,
        link_map: *mut c_void,
        offset: usize,
        address: *mut *mut c_void,
    ) -> c_int;
}

struct Agent<'a> {
    _process: Box<ProcessHandle<'a>>,
    raw: *mut ThreadAgent,
}

/// Why no agent could be created: the step that failed and its `td_err_e`.
struct AgentError {
    step: &'static str,
    code: c_int,
}

impl AgentError {
    fn message(&self) -> String {
        format!(
            "libthread_db {} failed with {}",
            self.step,
            td_error(self.code)
        )
    }
}

impl<'a> Agent<'a> {
    fn new(pid: Pid, services: &'a dyn ProcessServices) -> Result<Self, AgentError> {
        let mut process = Box::new(ProcessHandle {
            pid: pid.as_raw(),
            services,
        });
        let mut raw = ptr::null_mut();
        // SAFETY: td_init takes no arguments and only initializes
        // libthread_db's own state; callers hold `THREAD_DB`.
        let initialized = unsafe { td_init() };
        if initialized != TD_OK {
            return Err(AgentError {
                step: "initialization",
                code: initialized,
            });
        }
        // SAFETY: both pointers are valid for writes for the duration of the
        // call. libthread_db retains the process pointer, which stays valid
        // because the box's heap allocation does not move and `Agent` owns it
        // until after td_ta_delete.
        let created = unsafe { td_ta_new((&raw mut *process).cast(), &raw mut raw) };
        if created != TD_OK || raw.is_null() {
            return Err(AgentError {
                step: "agent creation",
                code: if created == TD_OK { TD_ERR } else { created },
            });
        }
        Ok(Self {
            _process: process,
            raw,
        })
    }
}

impl Drop for Agent<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` was produced by td_ta_new and is deleted exactly once
        // while its process handle is still alive.
        let _ = unsafe { td_ta_delete(self.raw) };
    }
}

/// Resolves a TLS address through `libthread_db`, or through glibc's own
/// layout descriptors when `libthread_db` refuses the inferior's C library
/// as another version than its own.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    process: Pid,
    thread: Pid,
    link_map: VirtualAddress,
    offset: u64,
) -> Result<VirtualAddress, Arc<str>> {
    let described =
        || glibc::tls_address(services, thread, link_map.get(), offset).map(VirtualAddress::new);
    if glibc::forced() {
        return described().map_err(|error| error.to_string().into());
    }
    let offset = usize::try_from(offset).map_err(|_| Arc::from("TLS offset exceeds usize"))?;
    let _serialized = THREAD_DB.lock().unwrap_or_else(PoisonError::into_inner);
    let agent = match Agent::new(process, services) {
        Ok(agent) => agent,
        Err(error) if error.code == TD_VERSION => {
            return described().map_err(|described| {
                format!(
                    "{}, and the C library's own descriptors could not locate it: {described}",
                    error.message()
                )
                .into()
            });
        }
        Err(error) => return Err(error.message().into()),
    };
    let mut handle = ThreadHandle {
        agent: ptr::null_mut(),
        unique: ptr::null_mut(),
    };
    // SAFETY: the agent is live, `handle` is writable, and the LWP belongs to
    // the process represented by the agent.
    let mapped = unsafe { td_ta_map_lwp2thr(agent.raw, thread.as_raw(), &raw mut handle) };
    if mapped != TD_OK {
        return Err(format!("libthread_db LWP lookup failed with {}", td_error(mapped)).into());
    }
    let mut address = ptr::null_mut();
    let link_map = usize::try_from(link_map.get())
        .map_err(|_| Arc::from("TLS module address exceeds usize"))?
        as *mut c_void;
    // SAFETY: `handle` came from this live agent, `link_map` was read from the
    // inferior's loader rendezvous, and `address` is valid for one pointer.
    let resolved =
        unsafe { td_thr_tls_get_addr(&raw const handle, link_map, offset, &raw mut address) };
    if resolved != TD_OK || address.is_null() {
        return Err(format!("libthread_db TLS lookup failed with {}", td_error(resolved)).into());
    }
    Ok(VirtualAddress::new(
        u64::try_from(address.addr()).expect("x86-64 pointer address fits u64"),
    ))
}

/// Names a `td_err_e` result and explains the failures that describe the
/// inferior rather than the debugger.
fn td_error(code: c_int) -> String {
    let name = usize::try_from(code)
        .ok()
        .and_then(|index| TD_ERRORS.get(index))
        .map_or_else(|| format!("error {code}"), |name| (*name).to_owned());
    let explanation = match code {
        // libthread_db only reads a C library of its own version, so a core
        // dump from another machine usually has none.
        TD_VERSION => "the inferior's C library is not the version of the debugger's libthread_db",
        TD_TLSDEFER => "the thread has not allocated the module's TLS block",
        TD_NOLIBTHREAD => {
            "no module defines glibc's thread library version, and the C library was \
             recognized neither as musl nor as statically linked glibc"
        }
        _ => return name,
    };
    format!("{name}: {explanation}")
}

fn with_process<T>(
    process: *mut c_void,
    operation: impl FnOnce(Pid, &dyn ProcessServices) -> T,
) -> Option<T> {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: libthread_db only calls process-service functions with the
        // exact boxed handle supplied to td_ta_new, which outlives the agent.
        let process = unsafe { process.cast::<ProcessHandle<'_>>().as_ref() }?;
        Some(operation(Pid::from_raw(process.pid), process.services))
    }))
    .ok()
    .flatten()
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_pdread(
    process: *mut c_void,
    address: *const c_void,
    output: *mut c_void,
    size: usize,
) -> c_int {
    if size == 0 {
        return PS_OK;
    }
    with_process(process, |_, services| {
        // SAFETY: libthread_db supplies a writable buffer of exactly `size`
        // bytes for the duration of this synchronous callback, and a nonzero
        // size rules out a null buffer.
        let output = unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), size) };
        if services.read(address.addr() as u64, output) {
            PS_OK
        } else {
            PS_ERR
        }
    })
    .unwrap_or(PS_ERR)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_ptread(
    process: *mut c_void,
    address: *const c_void,
    output: *mut c_void,
    size: usize,
) -> c_int {
    // SAFETY: this callback has the identical read contract as ps_pdread.
    unsafe { ps_pdread(process, address, output, size) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_pglobal_lookup(
    process: *mut c_void,
    object_name: *const c_char,
    symbol_name: *const c_char,
    address: *mut *mut c_void,
) -> c_int {
    if symbol_name.is_null() {
        return PS_ERR;
    }
    with_process(process, |_, services| {
        // A null object name, which libthread_db passes for the variables
        // of a statically linked C library, accepts any module.
        // SAFETY: libthread_db supplies valid NUL-terminated names, the
        // object's possibly null, and a writable result pointer for this
        // synchronous callback.
        let object_name = (!object_name.is_null())
            .then(|| unsafe { CStr::from_ptr(object_name) }.to_string_lossy());
        // SAFETY: same callback contract as `object_name` above.
        let symbol_name = unsafe { CStr::from_ptr(symbol_name) }.to_string_lossy();
        let Some(value) = services.lookup_symbol(object_name.as_deref(), &symbol_name) else {
            return PS_NOSYM;
        };
        let Ok(value) = usize::try_from(value) else {
            return PS_ERR;
        };
        // SAFETY: `address` points to one writable psaddr_t result.
        unsafe { address.write(value as *mut c_void) };
        PS_OK
    })
    .unwrap_or(PS_ERR)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_getpid(process: *mut c_void) -> c_int {
    with_process(process, |pid, _| pid.as_raw()).unwrap_or(-1)
}

const fn general_registers(native: &libc::user_regs_struct) -> [libc::c_long; X86_64_GREG_COUNT] {
    [
        native.r15.cast_signed(),
        native.r14.cast_signed(),
        native.r13.cast_signed(),
        native.r12.cast_signed(),
        native.rbp.cast_signed(),
        native.rbx.cast_signed(),
        native.r11.cast_signed(),
        native.r10.cast_signed(),
        native.r9.cast_signed(),
        native.r8.cast_signed(),
        native.rax.cast_signed(),
        native.rcx.cast_signed(),
        native.rdx.cast_signed(),
        native.rsi.cast_signed(),
        native.rdi.cast_signed(),
        native.orig_rax.cast_signed(),
        native.rip.cast_signed(),
        native.cs.cast_signed(),
        native.eflags.cast_signed(),
        native.rsp.cast_signed(),
        native.ss.cast_signed(),
        native.fs_base.cast_signed(),
        native.gs_base.cast_signed(),
        native.ds.cast_signed(),
        native.es.cast_signed(),
        native.fs.cast_signed(),
        native.gs.cast_signed(),
    ]
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_lgetregs(
    process: *mut c_void,
    lwp: c_int,
    output: *mut libc::c_long,
) -> c_int {
    with_process(process, |_, services| {
        let Some(registers) = services.registers(Pid::from_raw(lwp)) else {
            return PS_BADLID;
        };
        let registers = general_registers(&registers);
        // SAFETY: proc_service defines prgregset_t as 27 writable greg_t
        // elements on Linux x86-64.
        unsafe { ptr::copy_nonoverlapping(registers.as_ptr(), output, registers.len()) };
        PS_OK
    })
    .unwrap_or(PS_ERR)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_get_thread_area(
    process: *mut c_void,
    lwp: c_int,
    index: c_int,
    address: *mut *mut c_void,
) -> c_int {
    if index != X86_64_FS_INDEX {
        return PS_ERR;
    }
    with_process(process, |_, services| {
        let Some(registers) = services.registers(Pid::from_raw(lwp)) else {
            return PS_BADLID;
        };
        let Ok(fs_base) = usize::try_from(registers.fs_base) else {
            return PS_ERR;
        };
        // SAFETY: `address` is one writable psaddr_t result.
        unsafe { address.write(fs_base as *mut c_void) };
        PS_OK
    })
    .unwrap_or(PS_ERR)
}

macro_rules! unsupported_process_service {
    ($name:ident($($argument:ident: $type:ty),*)) => {
        #[unsafe(no_mangle)]
        const unsafe extern "C" fn $name($($argument: $type),*) -> c_int {
            $(let _ = $argument;)*
            PS_ERR
        }
    };
}

unsupported_process_service!(ps_pdwrite(process: *mut c_void, address: *mut c_void, input: *const c_void, size: usize));
unsupported_process_service!(ps_ptwrite(process: *mut c_void, address: *mut c_void, input: *const c_void, size: usize));
unsupported_process_service!(ps_lsetregs(process: *mut c_void, lwp: c_int, input: *const libc::c_long));
unsupported_process_service!(ps_lgetfpregs(process: *mut c_void, lwp: c_int, output: *mut c_void));
unsupported_process_service!(ps_lsetfpregs(process: *mut c_void, lwp: c_int, input: *const c_void));
unsupported_process_service!(ps_pstop(process: *mut c_void));
unsupported_process_service!(ps_pcontinue(process: *mut c_void));
unsupported_process_service!(ps_lstop(process: *mut c_void, lwp: c_int));
unsupported_process_service!(ps_lcontinue(process: *mut c_void, lwp: c_int));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_lookups_without_an_object_name_search_every_module() {
        struct Symbols;

        impl ProcessServices for Symbols {
            fn read(&self, _: u64, _: &mut [u8]) -> bool {
                false
            }

            fn registers(&self, _: Pid) -> Option<libc::user_regs_struct> {
                None
            }

            fn lookup_symbol(&self, object: Option<&str>, symbol: &str) -> Option<u64> {
                (object.is_none() && symbol == "_dl_stack_user").then_some(0x1234)
            }
        }

        let mut process = ProcessHandle {
            pid: 1,
            services: &Symbols,
        };
        let mut address = ptr::null_mut();
        // SAFETY: the handle and the result pointer are valid for the call,
        // and libthread_db passes no object name for a symbol a statically
        // linked program defines.
        let result = unsafe {
            ps_pglobal_lookup(
                (&raw mut process).cast(),
                ptr::null(),
                c"_dl_stack_user".as_ptr(),
                &raw mut address,
            )
        };
        assert_eq!((result, address.addr()), (PS_OK, 0x1234));
    }
}
