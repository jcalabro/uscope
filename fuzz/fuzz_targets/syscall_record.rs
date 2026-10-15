#![no_main]

use libfuzzer_sys::fuzz_target;
use microscope::syscall_engines::bpf;

fuzz_target!(|data: &[u8]| {
    let _ = bpf::decode(data);
});
