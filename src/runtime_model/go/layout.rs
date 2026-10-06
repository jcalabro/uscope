//! The runtime contract: every name the Go model reads, bound against the
//! runtime's own debug information.
//!
//! Offsets, sizes, and statuses come from DWARF by name, never from a table
//! of versions. A name the binary lacks makes the features that read it
//! unavailable, with a reason that names it; nothing guesses an offset.

use std::sync::Arc;

use super::super::{Member, RuntimeImage};
use crate::{ImageAddress, IntegerValue};

/// A missing name, which makes a feature unavailable.
pub type Missing = Arc<str>;

fn member(image: &dyn RuntimeImage, ty: &str, path: &[&str]) -> Result<Member, Missing> {
    image
        .member(ty, path)
        .ok_or_else(|| format!("the runtime has no member {ty}.{}", path.join(".")).into())
}

fn offset(image: &dyn RuntimeImage, ty: &str, path: &[&str], size: u64) -> Result<u64, Missing> {
    let found = member(image, ty, path)?;
    if found.size != size {
        return Err(format!(
            "{ty}.{} is {} bytes, not {size}",
            path.join("."),
            found.size
        )
        .into());
    }
    Ok(found.offset)
}

fn symbol(image: &dyn RuntimeImage, name: &str) -> Result<ImageAddress, Missing> {
    image
        .symbol(name)
        .map(|symbol| symbol.address)
        .ok_or_else(|| format!("the runtime has no symbol {name}").into())
}

fn constant(image: &dyn RuntimeImage, name: &str) -> Result<u64, Missing> {
    match image.constant(name) {
        Some(IntegerValue::Signed(value)) => u64::try_from(value).ok(),
        Some(IntegerValue::Unsigned(value)) => u64::try_from(value).ok(),
        _ => None,
    }
    .ok_or_else(|| format!("the runtime has no constant {name}").into())
}

/// A goroutine's status words, as `runtime._G*` names them.
#[derive(Debug, Clone, Copy)]
pub struct Statuses {
    pub idle: u64,
    pub runnable: u64,
    pub running: u64,
    pub syscall: u64,
    pub waiting: u64,
    pub dead: u64,
    pub copystack: u64,
    pub preempted: u64,
    /// The bit a garbage collector sets while it scans a stack; the rest of
    /// the word is the status the goroutine returns to.
    pub scan: u64,
    /// Statuses newer than every release the contract was checked on, such
    /// as `_Gleaked`, when the runtime has them.
    pub leaked: Option<u64>,
    pub dead_extra: Option<u64>,
}

impl Statuses {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let status = |name: &str| constant(image, &format!("runtime._G{name}"));
        Ok(Self {
            idle: status("idle")?,
            runnable: status("runnable")?,
            running: status("running")?,
            syscall: status("syscall")?,
            waiting: status("waiting")?,
            dead: status("dead")?,
            copystack: status("copystack")?,
            preempted: status("preempted")?,
            scan: status("scan")?,
            leaked: status("leaked").ok(),
            dead_extra: status("deadextra").ok(),
        })
    }
}

/// What reading the goroutine list needs.
#[derive(Debug, Clone)]
pub struct Goroutines {
    /// `runtime.allgs`, a slice of `*g`.
    pub allgs: ImageAddress,
    /// `runtime.allglen`, the published length of `allgs`.
    pub allglen: ImageAddress,
    pub status: u64,
    pub goid: u64,
    pub wait_reason: u64,
    pub m: u64,
    pub sched_pc: u64,
    pub gopc: u64,
    pub startpc: u64,
    pub parent_goid: Option<u64>,
    pub statuses: Statuses,
    /// The `procid` of an `m`: the thread it runs on.
    pub m_procid: u64,
    /// The goroutine an `m` runs.
    pub m_curg: u64,
}

impl Goroutines {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let g = |path: &[&str], size| offset(image, "runtime.g", path, size);
        Ok(Self {
            allgs: symbol(image, "runtime.allgs")?,
            allglen: symbol(image, "runtime.allglen")?,
            status: g(&["atomicstatus", "value"], 4)?,
            goid: g(&["goid"], 8)?,
            wait_reason: g(&["waitreason"], 1)?,
            m: g(&["m"], 8)?,
            sched_pc: g(&["sched", "pc"], 8)?,
            gopc: g(&["gopc"], 8)?,
            startpc: g(&["startpc"], 8)?,
            parent_goid: g(&["parentGoid"], 8).ok(),
            statuses: Statuses::bind(image)?,
            m_procid: offset(image, "runtime.m", &["procid"], 8)?,
            m_curg: offset(image, "runtime.m", &["curg"], 8)?,
        })
    }
}

/// What finding a thread's goroutine needs.
#[derive(Debug, Clone)]
pub struct Threads {
    /// Where the current goroutine is stored relative to a thread's
    /// thread pointer.
    pub tls_g: i64,
    pub g_m: u64,
    pub m_g0: u64,
    pub m_gsignal: u64,
    pub m_curg: u64,
}

impl Threads {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        // An executable linked by Go's own linker keeps g in the last word
        // of its thread-local block, just below the thread pointer.
        // External linking places it at `runtime.tlsg` within the block
        // instead, which needs the module's TLS layout.
        if image.symbol("runtime.tlsg").is_some() {
            return Err(
                "reading g through runtime.tlsg, as an externally linked program \
                        keeps it, is not supported yet"
                    .into(),
            );
        }
        let m = |path: &[&str]| offset(image, "runtime.m", path, 8);
        Ok(Self {
            tls_g: -8,
            g_m: offset(image, "runtime.g", &["m"], 8)?,
            m_g0: m(&["g0"])?,
            m_gsignal: m(&["gsignal"])?,
            m_curg: m(&["curg"])?,
        })
    }
}

/// Every part of the contract, each bound or missing on its own.
#[derive(Debug, Clone)]
pub struct Layout {
    pub goroutines: Result<Goroutines, Missing>,
    pub threads: Result<Threads, Missing>,
}

impl Layout {
    pub fn bind(image: &dyn RuntimeImage) -> Self {
        Self {
            goroutines: Goroutines::bind(image),
            threads: Threads::bind(image),
        }
    }
}
