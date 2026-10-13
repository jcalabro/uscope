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
/// An event that counts nothing, for the records of mappings and threads.
pub const COUNT_SW_DUMMY: u64 = 9;
/// A write breakpoint, which only the simulator's conformance tests set.
#[cfg(test)]
pub const BREAKPOINT_WRITE: u32 = 2;
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
    pub const CONTEXT_SWITCH: u64 = 1 << 26;
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
const IOC_SET_OUTPUT: libc::c_ulong = 0x2405;

/// `read_format`: each read also gives how many samples the event lost.
pub const FORMAT_LOST: u64 = 1 << 4;

/// An event's count, with those of the events inherited from it, and what
/// it has lost: every sample it and they found no room for, whether or not
/// a loss record has said so yet. The event reads [`FORMAT_LOST`].
pub fn counts(fd: &OwnedFd) -> std::io::Result<(u64, u64)> {
    let mut bytes = [0_u8; 16];
    let read = rustix::io::read(fd, &mut bytes[..])?;
    if read != bytes.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("a perf event's counts read {read} of 16 bytes"),
        ));
    }
    let (count, lost) = bytes.split_at(8);
    let word = |bytes: &[u8]| u64::from_ne_bytes(bytes.try_into().expect("eight bytes"));
    Ok((word(count), word(lost)))
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

/// Sends an event's records to the ring of `ring`'s event, which must be
/// on the same CPU.
pub fn set_output(fd: &OwnedFd, ring: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: the request takes the descriptor of another perf event, and
    // both descriptors are perf events this process owns.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_SET_OUTPUT, ring.as_raw_fd()) };
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
    rustix::param::page_size()
}

/// Whether `fd` is readable now, as a process descriptor is once its
/// process has exited.
pub fn readable(fd: &OwnedFd) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&now)).is_ok_and(|ready| ready > 0)
        && fds[0].revents().contains(PollFlags::IN)
}

/// Waits until `fd` is readable, as a process's descriptor is once it
/// has exited.
pub fn wait_readable(fd: &OwnedFd) {
    use rustix::event::{PollFd, PollFlags};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    while rustix::event::poll(&mut fds, None) == Err(rustix::io::Errno::INTR) {}
}

/// Waits until one of `rings` has data past its watermark, or `timeout`.
pub fn poll(rings: &[Ring], timeout: std::time::Duration) {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let mut fds: Vec<PollFd<'_>> = rings
        .iter()
        .map(|ring| PollFd::new(&ring.fd, PollFlags::IN))
        .collect();
    let timeout = Timespec::try_from(timeout).ok();
    let _ = rustix::event::poll(&mut fds, timeout.as_ref());
}

/// The time-stamp counter.
pub fn tsc() -> u64 {
    // SAFETY: every x86-64 processor reads its time-stamp counter.
    unsafe { core::arch::x86_64::_rdtsc() }
}
