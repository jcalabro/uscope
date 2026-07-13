#![no_main]
#![no_std]

use core::sync::atomic::{AtomicI32, Ordering};

static RUST_SINK: AtomicI32 = AtomicI32::new(0);

#[inline(never)]
fn inspect_scalars(
    flag: bool,
    signed_value: i32,
    unsigned_value: u64,
    single: f32,
    double_precision: f64,
) -> bool {
    let local_flag = !flag;
    let local_signed = signed_value + 1;
    let local_unsigned = unsigned_value + 2;
    let local_single = single + 0.5;
    let local_double = double_precision - 0.25;
    RUST_SINK.store(local_signed, Ordering::Relaxed);
    !local_flag
        && local_signed == -41
        && local_unsigned == 44
        && local_single == 1.75
        && local_double == -2.75
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let succeeded = inspect_scalars(
        core::hint::black_box(true),
        core::hint::black_box(-42),
        core::hint::black_box(42),
        core::hint::black_box(1.25),
        core::hint::black_box(-2.5),
    );
    i32::from(!succeeded)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

// libcore retains this symbol even with aborting panics; it is never called.
#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
