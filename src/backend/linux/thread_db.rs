#![allow(unsafe_code)]

//! Narrow glibc `libthread_db` boundary for ABI-correct TLS lookup.
//!
//! `libthread_db` is deliberately the only unsafe boundary in uscope. Its C
//! process-service callbacks are synchronous, read-only, bounded, and operate
//! on a process already owned by the ptrace controller.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::IoSliceMut;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;

use nix::libc;
use nix::sys::ptrace;
use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use object::{Object, ObjectSymbol};

use super::{mapped_module_load_bias, module_mappings};
use crate::VirtualAddress;

const TD_OK: c_int = 0;
const PS_OK: c_int = 0;
const PS_ERR: c_int = 1;
const PS_BADLID: c_int = 3;
const PS_NOSYM: c_int = 5;
const X86_64_GREG_COUNT: usize = 27;

#[repr(C)]
struct ProcessHandle {
    pid: c_int,
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
    fn td_ta_new(process: *mut ProcessHandle, agent: *mut *mut ThreadAgent) -> c_int;
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

struct Agent {
    _process: Box<ProcessHandle>,
    raw: *mut ThreadAgent,
}

impl Agent {
    fn new(pid: Pid) -> Result<Self, Arc<str>> {
        let mut process = Box::new(ProcessHandle { pid: pid.as_raw() });
        let mut raw = ptr::null_mut();
        // SAFETY: `process` is boxed before its stable address is passed to
        // libthread_db and remains owned by `Agent` until after td_ta_delete.
        let initialized = unsafe { td_init() };
        if initialized != TD_OK {
            return Err(format!("libthread_db initialization failed with {initialized}").into());
        }
        // SAFETY: both pointers are valid for writes for the duration of the
        // call; libthread_db retains only the stable boxed process pointer.
        let created = unsafe { td_ta_new(process.as_mut(), &raw mut raw) };
        if created != TD_OK || raw.is_null() {
            return Err(format!("libthread_db agent creation failed with {created}").into());
        }
        Ok(Self {
            _process: process,
            raw,
        })
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // SAFETY: `raw` was produced by td_ta_new and is deleted exactly once
        // while its process handle is still alive.
        let _ = unsafe { td_ta_delete(self.raw) };
    }
}

pub(super) fn tls_address(
    process: Pid,
    thread: Pid,
    link_map: VirtualAddress,
    offset: u64,
) -> Result<VirtualAddress, Arc<str>> {
    let offset = usize::try_from(offset).map_err(|_| Arc::from("TLS offset exceeds usize"))?;
    let agent = Agent::new(process)?;
    let mut handle = ThreadHandle {
        agent: ptr::null_mut(),
        unique: ptr::null_mut(),
    };
    // SAFETY: the agent is live, `handle` is writable, and the LWP belongs to
    // the process represented by the agent.
    let mapped = unsafe { td_ta_map_lwp2thr(agent.raw, thread.as_raw(), &raw mut handle) };
    if mapped != TD_OK {
        return Err(format!("libthread_db LWP lookup failed with {mapped}").into());
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
        return Err(format!("libthread_db TLS lookup failed with {resolved}").into());
    }
    Ok(VirtualAddress::new(
        u64::try_from(address.addr()).expect("x86-64 pointer address fits u64"),
    ))
}

fn with_process<T>(process: *mut ProcessHandle, operation: impl FnOnce(Pid) -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: libthread_db only calls process-service functions with the
        // exact boxed handle supplied to td_ta_new.
        let process = unsafe { process.as_ref() }?;
        Some(operation(Pid::from_raw(process.pid)))
    }))
    .ok()
    .flatten()
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

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_pdread(
    process: *mut ProcessHandle,
    address: *const c_void,
    output: *mut c_void,
    size: usize,
) -> c_int {
    with_process(process, |pid| {
        // SAFETY: libthread_db supplies a writable buffer of exactly `size`
        // bytes for the duration of this synchronous callback.
        let output = unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), size) };
        if read_process(pid, address as usize, output) {
            PS_OK
        } else {
            PS_ERR
        }
    })
    .unwrap_or(PS_ERR)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_ptread(
    process: *mut ProcessHandle,
    address: *const c_void,
    output: *mut c_void,
    size: usize,
) -> c_int {
    // SAFETY: this callback has the identical read contract as ps_pdread.
    unsafe { ps_pdread(process, address, output, size) }
}

fn lookup_symbol(pid: Pid, requested_object: &str, requested_symbol: &str) -> Option<u64> {
    let mappings = module_mappings(pid).ok()?;
    let mut fallback = None;
    for mapping in mappings {
        let Some(file_name) = mapping.path.file_name() else {
            continue;
        };
        let file_name = file_name.to_string_lossy();
        let preferred = requested_object.is_empty()
            || file_name == requested_object
            || file_name.starts_with(requested_object);
        let Ok(bias) = mapped_module_load_bias(&mapping) else {
            continue;
        };
        let Ok(data) = std::fs::read(&mapping.path) else {
            continue;
        };
        let Ok(object) = object::File::parse(data.as_slice()) else {
            continue;
        };
        let Some(symbol) = object
            .dynamic_symbols()
            .chain(object.symbols())
            .find(|symbol| symbol.name().ok() == Some(requested_symbol))
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

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_pglobal_lookup(
    process: *mut ProcessHandle,
    object_name: *const c_char,
    symbol_name: *const c_char,
    address: *mut *mut c_void,
) -> c_int {
    with_process(process, |pid| {
        // SAFETY: libthread_db supplies valid NUL-terminated names and a
        // writable result pointer for this synchronous callback.
        let object_name = unsafe { CStr::from_ptr(object_name) }.to_string_lossy();
        // SAFETY: same callback contract as `object_name` above.
        let symbol_name = unsafe { CStr::from_ptr(symbol_name) }.to_string_lossy();
        let Some(value) = lookup_symbol(pid, &object_name, &symbol_name) else {
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
unsafe extern "C" fn ps_getpid(process: *mut ProcessHandle) -> c_int {
    with_process(process, Pid::as_raw).unwrap_or(-1)
}

fn general_registers(pid: Pid) -> Option<[libc::c_long; X86_64_GREG_COUNT]> {
    let native = ptrace::getregs(pid).ok()?;
    Some([
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
    ])
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_lgetregs(
    process: *mut ProcessHandle,
    lwp: c_int,
    output: *mut libc::c_long,
) -> c_int {
    with_process(process, |_| {
        let Some(registers) = general_registers(Pid::from_raw(lwp)) else {
            return PS_BADLID;
        };
        // SAFETY: proc_service defines prgregset_t as 27 writable greg_t
        // elements on Linux x86-64.
        unsafe { ptr::copy_nonoverlapping(registers.as_ptr(), output, registers.len()) };
        PS_OK
    })
    .unwrap_or(PS_ERR)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn ps_get_thread_area(
    process: *mut ProcessHandle,
    lwp: c_int,
    _index: c_int,
    address: *mut *mut c_void,
) -> c_int {
    with_process(process, |_| {
        let Ok(registers) = ptrace::getregs(Pid::from_raw(lwp)) else {
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

unsupported_process_service!(ps_pdwrite(process: *mut ProcessHandle, address: *mut c_void, input: *const c_void, size: usize));
unsupported_process_service!(ps_ptwrite(process: *mut ProcessHandle, address: *mut c_void, input: *const c_void, size: usize));
unsupported_process_service!(ps_lsetregs(process: *mut ProcessHandle, lwp: c_int, input: *const libc::c_long));
unsupported_process_service!(ps_lgetfpregs(process: *mut ProcessHandle, lwp: c_int, output: *mut c_void));
unsupported_process_service!(ps_lsetfpregs(process: *mut ProcessHandle, lwp: c_int, input: *const c_void));
unsupported_process_service!(ps_pstop(process: *mut ProcessHandle));
unsupported_process_service!(ps_pcontinue(process: *mut ProcessHandle));
unsupported_process_service!(ps_lstop(process: *mut ProcessHandle, lwp: c_int));
unsupported_process_service!(ps_lcontinue(process: *mut ProcessHandle, lwp: c_int));
