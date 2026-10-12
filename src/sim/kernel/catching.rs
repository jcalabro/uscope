//! What the kernel follows of the client's catchpoints: the system calls
//! the debugged process makes that a catchpoint must stop, and those it
//! made without a system-call-stop to report them.

use super::Tid;
use crate::sim::cpu::{RAX, RDI, Registers};

/// A catchpoint as the client knows it: the calls it catches, every one
/// when empty, at which ends of them, and the condition it judges, of
/// those the client chooses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCatch {
    pub calls: Vec<u64>,
    pub entry: bool,
    pub exit: bool,
    /// `$arg0 == value`.
    pub arg0: Option<u64>,
    /// `$ret == value`, which a call's entry cannot judge, and so stops.
    pub ret: Option<i64>,
}

impl UserCatch {
    /// Whether the catchpoint stops call `number` at its entry or exit,
    /// made with `registers`.
    #[must_use]
    pub fn stops(&self, number: u64, registers: &Registers, exit: bool) -> bool {
        (self.calls.is_empty() || self.calls.contains(&number))
            && if exit { self.exit } else { self.entry }
            && self
                .arg0
                .is_none_or(|value| registers.general[RDI] == value)
            && self
                .ret
                .is_none_or(|value| !exit || registers.general[RAX].cast_signed() == value)
    }
}

/// A call a catchpoint stops, made by a traced thread of the debugged
/// process without a system-call-stop: one the debugger could not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenCatch {
    pub tid: Tid,
    pub call: u64,
    pub exit: bool,
}

/// The client's catchpoints and the calls they missed.
#[derive(Debug, Default)]
pub struct Catching {
    pub rules: Vec<UserCatch>,
    pub unseen: Vec<UnseenCatch>,
}

impl Catching {
    /// Whether any catchpoint stops call `number` at this end.
    #[must_use]
    pub fn stops(&self, number: u64, registers: &Registers, exit: bool) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.stops(number, registers, exit))
    }
}
