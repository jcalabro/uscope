//! Debug Adapter Protocol scenarios: the real `uscope dap` driven over
//! stdio the way editors drive it.

#[path = "../support/dap.rs"]
mod dap;
#[path = "../support/mod.rs"]
mod support;

mod breakpoints;
mod console;
mod execution;
mod lifecycle;
mod output;
mod variables;
