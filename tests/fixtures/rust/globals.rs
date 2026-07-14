#![no_main]
#![no_std]

use core::sync::atomic::{AtomicI32, Ordering};

static ROOT_IMMUTABLE: i32 = 141;
static mut ROOT_MUTABLE: i32 = 142;
static POINTER_VALUE: i32 = 157;
static mut ROOT_POINTER: *const i32 = &POINTER_VALUE;
static GLOBAL_SINK: AtomicI32 = AtomicI32::new(0);
static OPTIMIZED_AWAY: i32 = 149;

mod alpha {
    pub static DUPLICATE: i32 = 151;
}

mod beta {
    pub static DUPLICATE: i32 = 152;
}

#[inline(never)]
fn inspect_globals() {
    let mutable = unsafe { ROOT_MUTABLE };
    let pointer = unsafe { core::ptr::read_volatile(&raw const ROOT_POINTER) };
    let pointed = unsafe { core::ptr::read_volatile(pointer) };
    GLOBAL_SINK.store(
        ROOT_IMMUTABLE + mutable + alpha::DUPLICATE + beta::DUPLICATE + pointed,
        Ordering::Relaxed,
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    inspect_globals();
    let _ = OPTIMIZED_AWAY;
    i32::from(GLOBAL_SINK.load(Ordering::Relaxed) != 743)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
