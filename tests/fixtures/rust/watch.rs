#![no_main]
#![no_std]

static mut WATCHED: u64 = 0;

#[inline(never)]
fn watch_ready() {
    unsafe { core::ptr::write_volatile(&raw mut WATCHED, 1) };
}

#[inline(never)]
fn watched_writes() {
    for value in [2_u64, 3, 3] {
        unsafe { core::ptr::write_volatile(&raw mut WATCHED, value) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    watch_ready();
    watched_writes();
    i32::from(unsafe { core::ptr::read_volatile(&raw const WATCHED) } != 3)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
