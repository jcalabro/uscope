#![no_main]

use libfuzzer_sys::fuzz_target;
use microscope::syscalls::filter::Filter;

fuzz_target!(|text: &str| {
    let expressions: Vec<String> = text.lines().map(str::to_owned).collect();
    let _ = Filter::parse(&expressions, &|name: &str| name.parse().ok());
});
