#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    microscope::fuzz_glibc_tls(data);
});
