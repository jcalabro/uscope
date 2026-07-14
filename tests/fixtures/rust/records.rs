#![no_main]
#![no_std]

#[derive(Clone, Copy)]
struct Inner {
    signed_value: i32,
    unsigned_value: u32,
}

#[derive(Clone, Copy)]
struct Outer {
    inner: Inner,
    values: [i32; 2],
}

static GLOBAL_RECORD: Outer = Outer {
    inner: Inner {
        signed_value: -7,
        unsigned_value: 9,
    },
    values: [20, 22],
};

#[inline(never)]
fn inspect_records(record: &Outer, records: &[Outer; 2], slice: &[Outer]) -> bool {
    core::hint::black_box((record, records, slice));
    record.inner.signed_value == -7
        && record.inner.unsigned_value == 9
        && records[1].values[1] == 44
        && slice[0].values[0] == 20
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let records = [
        GLOBAL_RECORD,
        Outer {
            inner: Inner {
                signed_value: 5,
                unsigned_value: 6,
            },
            values: [43, 44],
        },
    ];
    i32::from(!inspect_records(&GLOBAL_RECORD, &records, &records[..]))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
