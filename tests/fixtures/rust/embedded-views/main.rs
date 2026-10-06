//! A Rust program that carries views for its own types in its
//! `.debug_uscope_views` section, through the SDK's macro.

use std::hint::black_box;

uscope_views::uscope_views_file!("tests/fixtures/rust/embedded-views/main.views");

/// Tags, which their view presents as the names they hold.
pub struct Tags {
    names: Vec<&'static str>,
}

/// A temperature in degrees Celsius.
pub struct Celsius(f64);

#[inline(never)]
fn barrier(fixture: *const u8) {
    black_box(fixture);
}

fn main() {
    let tags = Tags {
        names: vec!["red", "green"],
    };
    let temperature = Celsius(21.5);
    black_box((&tags, &temperature));
    barrier(std::ptr::from_ref(&tags).cast());
    std::process::exit(i32::from(tags.names.len() != 2 || temperature.0 < 0.0));
}
