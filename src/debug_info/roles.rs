//! The roles code plays in unwinding and stepping, decided once from what
//! the debug information and symbol tables name. Nothing else in uscope
//! recognizes code by its name.

use crate::CodeRole;

/// The role of code that only a linker symbol describes.
pub fn symbol_role(name: &str) -> CodeRole {
    match name {
        // The process's entry point, which nothing calls.
        "_start" => CodeRole::Outermost,
        // glibc's `sa_restorer`, which a signal handler returns to.
        "__restore_rt" => CodeRole::SignalTrampoline,
        _ => CodeRole::Ordinary,
    }
}
