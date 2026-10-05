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

/// What evaluating an expression at a stop produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluation {
    /// A value, or the unavailable state that stands for it, with the part
    /// of the expression whose value the program state could not provide.
    Value {
        value: crate::InspectedValue,
        cause: Option<syntax::Span>,
    },
    /// `base[start..end]`: an array or slice and the half-open range of its
    /// elements to page through.
    Range {
        base: crate::InspectedValue,
        start: i128,
        end: i128,
    },
}

#[cfg(test)]
mod tests;
