//! The protocols' source files leave out what only std can run, such as
//! their own tests, under cfg(loom). Only this crate sets it: loom's own
//! dependencies build as they always do.

fn main() {
    println!("cargo::rustc-cfg=loom");
}
