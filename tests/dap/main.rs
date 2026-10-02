//! Debug Adapter Protocol scenarios: the real `uscope dap` driven over
//! stdio the way editors drive it.

#[path = "../support/dap.rs"]
mod dap;
#[path = "../support/mod.rs"]
mod support;

mod breakpoints;
mod chaos;
mod console;
mod differential;
mod execution;
mod faults;
#[cfg(debug_assertions)]
mod flight_recorder;
mod languages;
mod lifecycle;
mod memory;
mod output;
mod robustness;
mod sources;
mod terminal;
mod traffic;
mod variables;
mod watch;
mod writes;
