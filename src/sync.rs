//! The synchronization types that protocols checked under loom use: std's
//! here, and loom's where `loom/` builds the same source.

pub use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
pub use std::sync::{Condvar, Mutex};
