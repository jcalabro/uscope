//! What a run leaves behind: its trace, whose hash is the run's
//! fingerprint, and the failure that ended it, if any.

use std::collections::VecDeque;
use std::fmt;

use super::choices::{fnv1a, fnv1a_continue};

/// The lines a run recorded, one or more per action. Every line feeds the
/// fingerprint; only the newest `keep` lines are kept, unless all are.
pub struct Trace {
    lines: VecDeque<String>,
    keep: Option<usize>,
    fingerprint: u64,
    dropped: u64,
}

impl Trace {
    /// A trace keeping every line, or only the newest `keep`.
    #[must_use]
    pub const fn new(keep: Option<usize>) -> Self {
        Self {
            lines: VecDeque::new(),
            keep,
            fingerprint: fnv1a(b""),
            dropped: 0,
        }
    }

    pub fn line(&mut self, line: String) {
        self.fingerprint = fnv1a_continue(self.fingerprint, line.as_bytes());
        self.fingerprint = fnv1a_continue(self.fingerprint, b"\n");
        if self.keep.is_some_and(|keep| self.lines.len() == keep) {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    /// The FNV-1a hash of every line recorded. Equal fingerprints mean
    /// identical runs.
    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// The lines kept, and how many older ones were dropped.
    #[must_use]
    pub const fn lines(&self) -> (&VecDeque<String>, u64) {
        (&self.lines, self.dropped)
    }
}

/// What a failure means, and so what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The debugger disagreed with the truth, broke its protocol, hung, or
    /// panicked: write a red-first test outside the simulator, then fix.
    Debugger,
    /// The run needed something the simulation does not model: probe the
    /// real behavior, add the rule with its conformance test, then model it.
    ModelGap,
    /// The simulator broke its own invariants or panicked: fix the simulator.
    Simulator,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Debugger => "debugger",
            Self::ModelGap => "model gap",
            Self::Simulator => "simulator",
        })
    }
}

/// Why a run failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    /// The oracle or check that failed.
    pub check: &'static str,
    pub message: String,
    /// The action at which the run failed.
    pub step: u64,
}

impl Failure {
    #[must_use]
    pub fn debugger(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Debugger,
            check,
            message: message.into(),
            step: 0,
        }
    }

    #[must_use]
    pub fn model_gap(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::ModelGap,
            check: "model",
            message: message.into(),
            step: 0,
        }
    }

    #[must_use]
    pub fn simulator(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Simulator,
            check,
            message: message.into(),
            step: 0,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failure at #{}, {}: {}",
            self.kind, self.step, self.check, self.message
        )
    }
}
