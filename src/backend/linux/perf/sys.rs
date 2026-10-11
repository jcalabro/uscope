//! The perf event system calls and the ring buffers events write to. The
//! profiler's only unsafe code is here.

#![allow(unsafe_code, reason = "perf events are system calls and shared memory")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::libc;

/// `struct perf_event_attr` to its eighth version.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Attr {
    pub kind: u32,
    pub size: u32,
    pub config: u64,
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup_watermark: u32,
    pub bp_type: u32,
    pub bp_addr: u64,
    pub bp_len: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub reserved_2: u16,
    pub aux_sample_size: u32,
    pub aux_action: u32,
    pub sig_data: u64,
    pub config3: u64,
}

const _: () = assert!(std::mem::size_of::<Attr>() == 136);

pub const TYPE_SOFTWARE: u32 = 1;
pub const TYPE_BREAKPOINT: u32 = 5;
pub const COUNT_SW_CPU_CLOCK: u64 = 0;
pub const BREAKPOINT_EXECUTE: u32 = 4;

/// `perf_event_attr` flag bits.
pub mod flag {
    pub const DISABLED: u64 = 1 << 0;
    pub const INHERIT: u64 = 1 << 1;
    pub const EXCLUDE_KERNEL: u64 = 1 << 5;
    pub const EXCLUDE_HV: u64 = 1 << 6;
    pub const MMAP: u64 = 1 << 8;
    pub const COMM: u64 = 1 << 9;
    pub const ENABLE_ON_EXEC: u64 = 1 << 12;
    pub const TASK: u64 = 1 << 13;
    pub const WATERMARK: u64 = 1 << 14;
    pub const SAMPLE_ID_ALL: u64 = 1 << 18;
    pub const EXCLUDE_CALLCHAIN_KERNEL: u64 = 1 << 21;
    pub const MMAP2: u64 = 1 << 23;
    pub const COMM_EXEC: u64 = 1 << 24;
    pub const USE_CLOCKID: u64 = 1 << 25;
}

/// `perf_event_attr.sample_type` bits.
pub mod sample {
    pub const TID: u64 = 1 << 1;
    pub const TIME: u64 = 1 << 2;
    pub const CALLCHAIN: u64 = 1 << 5;
    pub const CPU: u64 = 1 << 7;
    pub const PERIOD: u64 = 1 << 8;
    pub const REGS_USER: u64 = 1 << 12;
    pub const STACK_USER: u64 = 1 << 13;
    pub const IDENTIFIER: u64 = 1 << 16;
}

const FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;
const IOC_ENABLE: libc::c_ulong = 0x2400;
const IOC_DISABLE: libc::c_ulong = 0x2401;

/// `read_format`: each read also gives how many samples the event lost.
pub const FORMAT_LOST: u64 = 1 << 4;

/// What an event with [`FORMAT_LOST`] has lost: every sample its own
/// events and those it is inherited by found no room for, whether or not
/// a loss record has said so yet.
pub fn lost(fd: &OwnedFd) -> std::io::Result<u64> {
    let mut values = [0_u64; 2];
    // SAFETY: the buffer holds the two words this read format reads, and
    // outlives the call.
    let read = unsafe { libc::read(fd.as_raw_fd(), values.as_mut_ptr().cast(), 16) };
    if read != 16 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(values[1])
}

/// Opens an event for `pid` on `cpu`.
pub fn open(attr: &Attr, pid: i32, cpu: i32) -> std::io::Result<OwnedFd> {
    // SAFETY: `attr` is a valid, initialized perf_event_attr of the size
    // it declares, and outlives the call, which writes nothing through it
    // but its size on E2BIG.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            std::ptr::from_ref(attr),
            pid,
            cpu,
            -1_i32,
            FLAG_FD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(|_| std::io::Error::from_raw_os_error(libc::EBADF))?;
    // SAFETY: the kernel just returned this descriptor, which nothing else
    // owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Enables or disables an event.
pub fn enable(fd: &OwnedFd, enabled: bool) -> std::io::Result<()> {
    let request = if enabled { IOC_ENABLE } else { IOC_DISABLE };
    // SAFETY: the request takes an integer argument, and the descriptor is
    // a perf event this process owns.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// An event's ring buffer: one metadata page, then a power of two of data
/// pages the kernel writes records to.
#[derive(Debug)]
pub struct Ring {
    fd: OwnedFd,
    base: std::ptr::NonNull<u8>,
    length: usize,
    data_size: usize,
}

// SAFETY: the mapping belongs to the ring alone, and every access to it
// goes through `&mut self`.
unsafe impl Send for Ring {}

/// `perf_event_mmap_page` offsets.
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;

impl Ring {
    /// Maps `pages` data pages, a power of two, for an event.
    pub fn map(fd: OwnedFd, pages: usize) -> std::io::Result<Self> {
        let page = page_size();
        let length = (pages + 1) * page;
        // SAFETY: a fresh shared mapping of the event's descriptor, which
        // the kernel sizes and owns; nothing else refers to it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let base = std::ptr::NonNull::new(base.cast::<u8>())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOMEM))?;
        Ok(Self {
            fd,
            base,
            length,
            data_size: pages * page,
        })
    }

    pub const fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    #[expect(
        clippy::cast_ptr_alignment,
        reason = "the metadata page is page aligned, and its words at multiples of eight"
    )]
    const fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: the metadata page is mapped for the ring's life, the
        // word is aligned, and the kernel accesses it atomically too.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset).cast::<u64>()) }
    }

    /// Appends every record the kernel has written since the last drain
    /// to `out` and returns their bytes to the kernel. Records are whole:
    /// the kernel publishes its head only after a record is complete.
    pub fn drain(&mut self, out: &mut Vec<u8>) -> usize {
        let head = self.word(DATA_HEAD).load(Ordering::Acquire);
        let tail = self.word(DATA_TAIL).load(Ordering::Relaxed);
        let size = self.data_size;
        let Ok(available) = usize::try_from(head.wrapping_sub(tail)) else {
            return 0;
        };
        if available == 0 || available > size {
            return 0;
        }
        let page = page_size();
        let start = usize::try_from(tail % size as u64).expect("an offset inside the ring");
        let first = available.min(size - start);
        out.reserve(available);
        // SAFETY: [tail, head) lies inside the data pages, which the kernel
        // does not write until the tail passes it; the destination has the
        // capacity just reserved, and the two never overlap.
        unsafe {
            let data = self.base.as_ptr().add(page);
            let destination = out.as_mut_ptr().add(out.len());
            std::ptr::copy_nonoverlapping(data.add(start), destination, first);
            std::ptr::copy_nonoverlapping(data, destination.add(first), available - first);
            out.set_len(out.len() + available);
        }
        self.word(DATA_TAIL).store(head, Ordering::Release);
        available
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the mapping was made with this address and length and is
        // unmapped once, here.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.length);
        }
    }
}

/// The system's page size.
pub fn page_size() -> usize {
    // SAFETY: sysconf reads a constant.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).unwrap_or(4096)
}

/// Waits until one of `rings` has data past its watermark, or `timeout`.
pub fn poll(rings: &[Ring], timeout: std::time::Duration) {
    let mut fds: Vec<libc::pollfd> = rings
        .iter()
        .map(|ring| libc::pollfd {
            fd: ring.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let timeout = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `fds` holds `fds.len()` initialized entries for the call.
    unsafe {
        libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout);
    }
}
