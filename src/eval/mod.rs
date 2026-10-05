//! The expression language: one small, exact language for reading and
//! changing program state, shared by every source language
//! (`docs/expressions.md`).
//!
//! This module is pure. It reaches a program only through the traits the
//! debugger implements for it, and never performs I/O, reads clocks, or
//! starts threads.

pub mod error;
pub mod number;
pub mod syntax;

#[cfg(test)]
mod tests;
