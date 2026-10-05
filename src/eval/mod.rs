//! The expression language: one small, exact language for reading and
//! changing program state, shared by every source language
//! (`docs/expressions.md`).
//!
//! This module is pure. It reaches a program only through the traits the
//! debugger implements for it, and never performs I/O, reads clocks, or
//! starts threads.

pub mod bind;
pub mod error;
#[cfg(test)]
mod fake;
pub mod interp;
pub mod ir;
pub mod number;
pub mod syntax;
pub mod target;
pub mod types;

#[cfg(test)]
mod tests;
