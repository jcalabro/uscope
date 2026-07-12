use std::sync::atomic::{AtomicI32, Ordering};

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

fn main() {
    let succeeded = inspect_scalars(
        std::hint::black_box(true),
        std::hint::black_box(-42),
        std::hint::black_box(42),
        std::hint::black_box(1.25),
        std::hint::black_box(-2.5),
    );
    std::process::exit(if succeeded { 0 } else { 1 });
}
