//! Tests that tie the simulation to the real machine. The kernel's rules
//! (K-*) are each run twice, natively and simulated, with the observations
//! required to agree; the CPU is single-stepped in lockstep with the real
//! one over every golden program.

mod cpu;
mod kernel;
mod tracee;
