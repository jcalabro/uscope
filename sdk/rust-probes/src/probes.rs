//! The runtime the macros call: the control block, each thread's ring, and
//! the paths that write them.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{
    AtomicPtr, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering, compiler_fence,
};
use std::sync::Once;

use crate::abi;

/// The control block uscope finds in the program's file and writes.
#[repr(C, align(64))]
pub struct Control {
    magic: [u8; 8],
    abi: u16,
    clock: AtomicU16,
    enable: AtomicU32,
    ring_size: AtomicU32,
    rings: AtomicU32,
    heartbeat: AtomicU64,
    dropped: AtomicU64,
    capacity: u32,
    exiting: AtomicU32,
    flushed: AtomicU32,
    timeout: AtomicU32,
    reserved: [u32; 2],
    registry: [AtomicPtr<Header>; abi::MOST_RINGS],
}

#[unsafe(link_section = "uscope_probe_control")]
#[used]
static CONTROL: Control = Control {
    magic: abi::MAGIC,
    abi: abi::ABI,
    clock: AtomicU16::new(abi::CLOCK_MONOTONIC),
    enable: AtomicU32::new(0),
    ring_size: AtomicU32::new(abi::DEFAULT_RING_SIZE),
    rings: AtomicU32::new(0),
    heartbeat: AtomicU64::new(0),
    dropped: AtomicU64::new(0),
    #[expect(clippy::cast_possible_truncation, reason = "the registry is small")]
    capacity: abi::MOST_RINGS as u32,
    exiting: AtomicU32::new(0),
    flushed: AtomicU32::new(0),
    timeout: AtomicU32::new(0),
    reserved: [0; 2],
    registry: [const { AtomicPtr::new(ptr::null_mut()) }; abi::MOST_RINGS],
};

const _: () = assert!(core::mem::offset_of!(Control, registry) == abi::CONTROL_SIZE);

/// A ring's header; its records follow it.
#[repr(C, align(128))]
pub struct Header {
    magic: u32,
    version: u8,
    size: u8,
    clock: AtomicU8,
    reserved: u8,
    thread: AtomicU32,
    state: AtomicU32,
    signal_drops: AtomicU64,
    line_0: [u8; 40],
    head: AtomicU64,
    /// Whether the thread is writing a record, which a signal handler that
    /// interrupts it must not.
    writing: AtomicU8,
    line_1: [u8; 55],
}

const _: () = assert!(core::mem::size_of::<Header>() == abi::RING_HEADER);
const _: () = assert!(core::mem::offset_of!(Header, head) == 64);

/// A thread whose events no ring takes: it has exited, or none was left.
const NO_RING: *mut Header = ptr::dangling_mut();

thread_local! {
    static RING: Cell<*mut Header> = const { Cell::new(ptr::null_mut()) };
    static RETIRE: Retire = const { Retire };
}

/// Retires the thread's ring when the thread exits.
struct Retire;

impl Drop for Retire {
    fn drop(&mut self) {
        let ring = RING.with(|ring| ring.replace(NO_RING));
        if ring != NO_RING && !ring.is_null() {
            // SAFETY: a ring, once allocated, is never freed.
            unsafe { &*ring }
                .state
                .store(abi::RING_RETIRED, Ordering::Release);
        }
    }
}

/// Whether uscope is recording the program's instrumentation: every site
/// it chose is on.
#[must_use]
pub fn listening() -> bool {
    CONTROL.enable.load(Ordering::Acquire) != 0
}

/// Whether the site whose state byte is `state` is on.
#[doc(hidden)]
#[inline(always)]
pub fn __on(state: &AtomicU8) -> bool {
    state.load(Ordering::Relaxed) != 0
}

/// What keeps a guard on the thread it began on, whose ring it ends in.
type ThreadBound = PhantomData<*const ()>;

/// The zone [`zone!`](crate::zone) opens, which ends when it drops.
#[must_use = "a zone ends when its guard drops"]
pub struct Zone(Option<&'static AtomicU8>, ThreadBound);

impl Zone {
    #[doc(hidden)]
    #[inline(always)]
    pub fn begin(state: &'static AtomicU8) -> Self {
        let began = __on(state) && write(state, abi::ZONE_BEGIN, 0, &[]);
        Self(began.then_some(state), PhantomData)
    }
}

impl Drop for Zone {
    #[inline(always)]
    fn drop(&mut self) {
        // A zone that began ends, whether or not its site is still on.
        if let Some(state) = self.0 {
            end(state, abi::ZONE_END);
        }
    }
}

/// The label [`label!`](crate::label) opens, which ends when it drops.
#[must_use = "a label ends when its guard drops"]
pub struct Label(Option<&'static AtomicU8>, ThreadBound);

impl Label {
    #[doc(hidden)]
    #[inline(always)]
    pub fn begin(state: &'static AtomicU8, value: &dyn fmt::Display) -> Self {
        let began = __on(state) && text(state, abi::LABEL_BEGIN, format_args!("{value}"));
        Self(began.then_some(state), PhantomData)
    }
}

impl Drop for Label {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(state) = self.0 {
            end(state, abi::LABEL_END);
        }
    }
}

/// Ends a zone or label out of line, so that a guard whose site was off
/// drops to one test, which the compiler then inlines.
#[inline(never)]
fn end(state: &'static AtomicU8, kind: u8) {
    write(state, kind, 0, &[]);
}

#[doc(hidden)]
#[inline(always)]
pub fn __event(state: &'static AtomicU8, kind: u8) {
    if __on(state) {
        write(state, kind, 0, &[]);
    }
}

#[doc(hidden)]
#[inline]
pub fn __plot(state: &'static AtomicU8, value: f64) {
    write(state, abi::PLOT, 0, &value.to_le_bytes());
}

#[doc(hidden)]
#[inline(never)]
pub fn __message(state: &'static AtomicU8, message: fmt::Arguments<'_>) {
    text(state, abi::MESSAGE, message);
}

/// Writes an event whose argument is `text`, cut to its longest.
#[inline(never)]
fn text(state: &'static AtomicU8, kind: u8, text: fmt::Arguments<'_>) -> bool {
    let mut cut = Cut {
        bytes: [0; abi::MOST_TEXT],
        length: 0,
    };
    // A cut text is the bytes that fit, as a full one would be.
    let _ = fmt::write(&mut cut, text);
    let text = &cut.bytes[..cut.length];
    #[expect(clippy::cast_possible_truncation, reason = "a text is short")]
    write(state, kind, text.len() as u32, text)
}

/// A text, cut where it would pass its longest, at a character's start.
struct Cut {
    bytes: [u8; abi::MOST_TEXT],
    length: usize,
}

impl fmt::Write for Cut {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = abi::MOST_TEXT - self.length;
        let mut take = text.len().min(room);
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        if take < text.len() { Err(fmt::Error) } else { Ok(()) }
    }
}

/// Writes one event of `kind` from the site whose state byte is `state` to
/// the thread's ring: a record, and `more` in the records after it.
/// Whether the ring took it.
#[inline(always)]
fn write(state: &'static AtomicU8, kind: u8, argument: u32, more: &[u8]) -> bool {
    let mut ring = RING.with(Cell::get);
    if ring.is_null() {
        ring = acquire();
    }
    if ring == NO_RING {
        CONTROL.dropped.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // SAFETY: a ring, once allocated, is never freed, and only its thread
    // writes its records.
    let header = unsafe { &*ring };
    if header.writing.load(Ordering::Relaxed) != 0 {
        header.signal_drops.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    header.writing.store(1, Ordering::Relaxed);
    compiler_fence(Ordering::SeqCst);
    let time = now(header.clock.load(Ordering::Relaxed));
    let head = header.head.load(Ordering::Relaxed);
    let mask = (1_u64 << header.size) - 1;
    // SAFETY: the records follow the header, `mask + 1` of them.
    let records = unsafe { ring.add(1) }.cast::<[u8; abi::RECORD]>();
    let site = site(state);
    let mut record = [0; abi::RECORD];
    record[..8].copy_from_slice(&time.to_le_bytes());
    record[8..12].copy_from_slice(&site.to_le_bytes());
    record[12..].copy_from_slice(&(u32::from(kind) | argument << 8).to_le_bytes());
    #[expect(clippy::cast_possible_truncation, reason = "a position is masked to the ring")]
    // SAFETY: the position is masked to the ring.
    unsafe {
        records.add((head & mask) as usize).write(record);
    }
    let mut position = head + 1;
    for chunk in more.chunks(15) {
        let mut record = [0; abi::RECORD];
        let (low, high) = chunk.split_at(chunk.len().min(12));
        record[..low.len()].copy_from_slice(low);
        record[12] = abi::CONTINUED;
        record[13..13 + high.len()].copy_from_slice(high);
        #[expect(clippy::cast_possible_truncation, reason = "a position is masked to the ring")]
        // SAFETY: the position is masked to the ring.
        unsafe {
            records.add((position & mask) as usize).write(record);
        }
        position += 1;
    }
    header.head.store(position, Ordering::Release);
    compiler_fence(Ordering::SeqCst);
    header.writing.store(0, Ordering::Relaxed);
    // Every so often, the thread asks whether uscope is still there.
    if (head ^ position) >> 10 != 0 {
        check_heartbeat();
    }
    true
}

/// A site's key: its state byte's offset from the control block.
#[inline(always)]
fn site(state: &'static AtomicU8) -> u32 {
    #[expect(clippy::cast_possible_truncation, reason = "a module is under 2 GiB")]
    let offset = (ptr::from_ref(state).addr() as i64 - ptr::from_ref(&CONTROL).addr() as i64) as i32;
    offset.cast_unsigned()
}

/// The time now, by `clock`.
#[inline(always)]
fn now(clock: u8) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if u16::from(clock) == abi::CLOCK_TSC {
        // SAFETY: every x86-64 processor reads its time-stamp counter.
        return unsafe { core::arch::x86_64::_rdtsc() };
    }
    let _ = clock;
    monotonic()
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanoseconds: i64,
}

unsafe extern "C" {
    fn clock_gettime(clock: i32, time: *mut Timespec) -> i32;
    fn gettid() -> i32;
    fn atexit(handler: extern "C" fn()) -> i32;
    fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> i32;
}

/// Nanoseconds of `CLOCK_MONOTONIC`.
fn monotonic() -> u64 {
    const CLOCK_MONOTONIC: i32 = 1;
    let mut time = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: the time is a valid timespec to write.
    unsafe { clock_gettime(CLOCK_MONOTONIC, &raw mut time) };
    (time.seconds.cast_unsigned())
        .wrapping_mul(1_000_000_000)
        .wrapping_add(time.nanoseconds.cast_unsigned())
}

/// Switches every site off once uscope has gone quiet.
#[cold]
fn check_heartbeat() {
    let heartbeat = CONTROL.heartbeat.load(Ordering::Relaxed);
    let timeout = match CONTROL.timeout.load(Ordering::Relaxed) {
        0 => abi::STALE_NANOSECONDS,
        milliseconds => u64::from(milliseconds) * 1_000_000,
    };
    if CONTROL.enable.load(Ordering::Relaxed) != 0
        && monotonic().saturating_sub(heartbeat) > timeout
    {
        disable();
    }
}

unsafe extern "C" {
    static __start_uscope_sites: [u8; 0];
    static __stop_uscope_sites: [u8; 0];
}

// The SDK's own site, so that the section is always there.
static SITE: &AtomicU8 = crate::__site!(crate::abi::SITE_NONE, "uscope_probes");

/// Switches every site off.
fn disable() {
    CONTROL.enable.store(0, Ordering::Relaxed);
    let start = (&raw const __start_uscope_sites).cast::<u8>();
    let stop = (&raw const __stop_uscope_sites).cast::<u8>();
    let mut site = start;
    while site < stop {
        // SAFETY: the section is a table of sites, each of whose state
        // offsets names an `AtomicU8` in the same module.
        unsafe {
            let field = site.add(24);
            let offset = field.cast::<i32>().read_unaligned();
            let state = &*field.offset(offset as isize).cast::<AtomicU8>();
            state.store(0, Ordering::Relaxed);
            site = site.add(abi::SITE_SIZE);
        }
    }
    let _ = SITE;
}

static ATFORK: Once = Once::new();

/// A child of `fork` records nothing until uscope says so, and never into
/// its parent's rings.
extern "C" fn forked() {
    disable();
    for ring in &CONTROL.registry {
        ring.store(ptr::null_mut(), Ordering::Relaxed);
    }
    CONTROL.rings.store(0, Ordering::Relaxed);
    let _ = RING.try_with(|ring| ring.set(ptr::null_mut()));
}

/// A program that exits while uscope reads it waits a moment for uscope to
/// read what its threads wrote last.
extern "C" fn exiting() {
    let fresh = |now: u64| {
        now.saturating_sub(CONTROL.heartbeat.load(Ordering::Relaxed)) < abi::FRESH_NANOSECONDS
    };
    let start = monotonic();
    if CONTROL.enable.load(Ordering::Relaxed) == 0 || !fresh(start) {
        return;
    }
    CONTROL.exiting.store(1, Ordering::Release);
    while CONTROL.flushed.load(Ordering::Acquire) == 0
        && monotonic().saturating_sub(start) < abi::EXIT_WAIT_NANOSECONDS
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// The thread's ring, the first time it writes one: a free one, or a new
/// one, or none.
#[cold]
#[inline(never)]
fn acquire() -> *mut Header {
    if CONTROL.enable.load(Ordering::Acquire) == 0 {
        return NO_RING;
    }
    ATFORK.call_once(|| {
        // SAFETY: the handlers touch only atomics, the thread's own slot,
        // and the clock.
        unsafe {
            pthread_atfork(None, None, Some(forked));
            atexit(exiting);
        }
    });
    // A thread that is exiting keeps no ring.
    if RETIRE.try_with(|_| ()).is_err() {
        return NO_RING;
    }
    let ring = reuse().unwrap_or_else(allocate);
    if !ring.is_null() && ring != NO_RING {
        // SAFETY: the thread claimed the ring, which is never freed.
        let header = unsafe { &*ring };
        // SAFETY: gettid always succeeds.
        header.thread.store(unsafe { gettid() }.cast_unsigned(), Ordering::Relaxed);
        #[expect(clippy::cast_possible_truncation, reason = "a clock is a small number")]
        header
            .clock
            .store(CONTROL.clock.load(Ordering::Relaxed) as u8, Ordering::Relaxed);
        header.state.store(abi::RING_ACTIVE, Ordering::Release);
    }
    let ring = if ring.is_null() { NO_RING } else { ring };
    RING.with(|slot| slot.set(ring));
    ring
}

/// A ring a thread that exited left, which uscope has read.
fn reuse() -> Option<*mut Header> {
    let rings = CONTROL.rings.load(Ordering::Acquire) as usize;
    CONTROL.registry[..rings.min(abi::MOST_RINGS)]
        .iter()
        .map(|slot| slot.load(Ordering::Acquire))
        .find(|&ring| {
            !ring.is_null()
                // SAFETY: a ring, once allocated, is never freed.
                && unsafe { &*ring }
                    .state
                    .compare_exchange(
                        abi::RING_FREE,
                        abi::RING_CLAIMED,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
        })
}

/// A new ring of the size uscope chose, in the registry, or null.
fn allocate() -> *mut Header {
    let mut index = CONTROL.rings.load(Ordering::Relaxed);
    loop {
        if index as usize >= abi::MOST_RINGS {
            return ptr::null_mut();
        }
        match CONTROL.rings.compare_exchange_weak(
            index,
            index + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(now) => index = now,
        }
    }
    let size = CONTROL
        .ring_size
        .load(Ordering::Relaxed)
        .clamp(abi::LEAST_RING_SIZE, abi::MOST_RING_SIZE);
    let Ok(layout) = std::alloc::Layout::from_size_align(
        abi::RING_HEADER + (abi::RECORD << size),
        abi::RING_HEADER,
    ) else {
        return ptr::null_mut();
    };
    // SAFETY: the layout has a size.
    let ring = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<Header>();
    if ring.is_null() {
        return ring;
    }
    // SAFETY: the allocation is the ring's, zeroed, and no other thread
    // sees it before the registry holds it.
    unsafe {
        (&raw mut (*ring).magic).write(abi::RING_MAGIC);
        (&raw mut (*ring).version).write(abi::RING_VERSION);
        #[expect(clippy::cast_possible_truncation, reason = "a size is clamped")]
        (&raw mut (*ring).size).write(size as u8);
        (*ring).state.store(abi::RING_CLAIMED, Ordering::Relaxed);
    }
    CONTROL.registry[index as usize].store(ring, Ordering::Release);
    ring
}
