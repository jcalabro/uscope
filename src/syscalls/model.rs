//! The neutral events both engines produce. Nothing here names ELF, BPF,
//! or ptrace: an engine translates what it sees into these types at its
//! edge, and everything after that is written once.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::table;

/// The calling convention a system call was made with. One process may make
/// calls under several, as a 64-bit process may through `int 0x80`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Abi {
    /// The x86-64 `syscall` instruction.
    X86_64,
    /// The i386 ABI: `int 0x80` or `sysenter`, from 32-bit or 64-bit code.
    X86_32,
    /// The x32 ABI: `syscall` with bit 30 set in the number.
    X32,
}

impl Abi {
    pub const ALL: [Self; 3] = [Self::X86_64, Self::X86_32, Self::X32];

    /// strace's personality suffix for the ABI, as in `open@64`.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::X86_64 => "64",
            Self::X86_32 => "32",
            Self::X32 => "x32",
        }
    }

    #[must_use]
    pub fn from_suffix(suffix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|abi| abi.suffix() == suffix)
    }

    /// Bytes in a pointer or `long` under the ABI.
    #[must_use]
    pub const fn word_bytes(self) -> u8 {
        match self {
            Self::X86_64 => 8,
            Self::X86_32 | Self::X32 => 4,
        }
    }
}

/// One system call: a number under an ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SyscallId {
    pub abi: Abi,
    pub number: u32,
}

impl SyscallId {
    #[must_use]
    pub const fn new(abi: Abi, number: u32) -> Self {
        Self { abi, number }
    }

    /// The call's name in its ABI's table, or `None` for a number the
    /// table does not list.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        table::name(self)
    }

    /// The name, or `syscall_0x1c8` for a number with none, as strace
    /// prints it.
    #[must_use]
    pub fn display_name(self) -> std::borrow::Cow<'static, str> {
        self.name().map_or_else(
            || format!("syscall_{:#x}", self.number).into(),
            std::borrow::Cow::Borrowed,
        )
    }

    /// The call named `name` under `abi`.
    #[must_use]
    pub fn named(abi: Abi, name: &str) -> Option<Self> {
        table::number(abi, name).map(|number| Self::new(abi, number))
    }
}

/// An error number a call failed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Errno(pub u16);

/// The largest error number the kernel returns: a return value in
/// `-4095..0` is an error, and any other is a result.
pub const MAX_ERRNO: i64 = 4095;

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// It returned this value.
    Returned(i64),
    /// It failed with this error.
    Failed(Errno),
    /// It had not returned when the trace ended or the thread was last
    /// seen: still blocked, or the trace stopped first.
    Unfinished,
    /// Its process exited, or replaced itself by `execve`, before the call
    /// returned, as `exit_group` and a successful `execve` never return.
    ProcessExited,
    /// The engine could not tell, as when the exit was lost.
    Unavailable,
}

impl Outcome {
    /// Classifies a raw return value the way the kernel's convention
    /// means it: `-4095..0` is an error number.
    #[must_use]
    pub fn of_return(value: i64) -> Self {
        if (-MAX_ERRNO..0).contains(&value) {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the range checked above fits"
            )]
            Self::Failed(Errno((-value) as u16))
        } else {
            Self::Returned(value)
        }
    }

    #[must_use]
    pub const fn errno(self) -> Option<Errno> {
        match self {
            Self::Failed(errno) => Some(errno),
            _ => None,
        }
    }

    /// The raw return value, when the call returned at all.
    #[must_use]
    pub fn raw(self) -> Option<i64> {
        match self {
            Self::Returned(value) => Some(value),
            Self::Failed(Errno(errno)) => Some(-i64::from(errno)),
            _ => None,
        }
    }
}

/// When an engine read the memory behind a pointer argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadAt {
    /// Before the kernel ran the call: what it was asked to do.
    Entry,
    /// After the call returned. Another thread may have changed the memory
    /// in between, and an input buffer then shows what it holds now.
    Exit,
}

/// How much of the memory behind a pointer argument an engine captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureState {
    /// All of it.
    Complete,
    /// The first bytes, up to the string limit; `wanted` bytes were there,
    /// or more when it is `u32::MAX`.
    Truncated { wanted: u32 },
    /// The memory could not be read: unmapped, or not yet paged in.
    Unreadable,
    /// The selection asked for no copy.
    NotRequested,
}

/// Bytes an engine copied from behind one pointer argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    /// The argument, from 0.
    pub arg: u8,
    pub when: ReadAt,
    pub state: CaptureState,
    pub bytes: Arc<[u8]>,
}

/// Facts about an event that change how far it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EventFlags(pub u8);

impl EventFlags {
    /// The thread was stopped in the debugger while it was in the call, so
    /// the duration includes the stop.
    pub const SPANNED_STOP: Self = Self(1);
    /// The entry was not seen: tracing began during the call, or the call
    /// was refused before it entered. It has no duration.
    pub const ENTRY_UNSEEN: Self = Self(2);
    /// The engine could not tell which ABI made the call, and named it by
    /// the 64-bit table.
    pub const ABI_UNCERTAIN: Self = Self(4);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One system call one thread made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallEvent {
    /// The thread, by its id in the traced process's pid namespace.
    pub thread: u32,
    /// Its process.
    pub process: u32,
    pub call: SyscallId,
    /// The six argument registers, always, however many the call takes.
    pub args: [u64; 6],
    /// Bytes behind pointer arguments, in argument order.
    pub captured: Vec<Capture>,
    pub outcome: Outcome,
    /// `CLOCK_MONOTONIC` nanoseconds at entry, when it was seen.
    pub entered: Option<u64>,
    /// `CLOCK_MONOTONIC` nanoseconds at exit, when it was seen.
    pub exited: Option<u64>,
    /// Return addresses in the thread's user stack, innermost first, when
    /// stacks were asked for.
    pub stack: Option<Arc<[u64]>>,
    pub flags: EventFlags,
}

impl SyscallEvent {
    /// Nanoseconds from entry to exit, when both were seen.
    #[must_use]
    pub fn duration(&self) -> Option<u64> {
        Some(self.exited?.saturating_sub(self.entered?))
    }

    /// The time the event is ordered by: its exit, or its entry for a call
    /// that has not returned.
    #[must_use]
    pub fn time(&self) -> u64 {
        self.exited.or(self.entered).unwrap_or(0)
    }

    #[must_use]
    pub fn capture(&self, arg: u8) -> Option<&Capture> {
        self.captured.iter().find(|capture| capture.arg == arg)
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessEnd {
    Exited(i32),
    Killed { signal: i32, core: bool },
}

/// Everything a trace holds besides calls, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceEvent {
    Call(SyscallEvent),
    /// A signal was delivered to a thread.
    Signal {
        thread: u32,
        process: u32,
        time: u64,
        signal: i32,
        /// `si_code`, when the engine saw it.
        code: Option<i32>,
        /// The sending process, for signals one sent.
        sender: Option<u32>,
    },
    /// A thread created a process or a thread.
    Spawn {
        parent_thread: u32,
        parent: u32,
        child: u32,
        /// Whether the child is a thread of the parent's process.
        thread: bool,
        time: u64,
    },
    /// A process replaced its image.
    Exec {
        thread: u32,
        process: u32,
        time: u64,
        path: Arc<str>,
    },
    /// A process ended.
    Exit {
        process: u32,
        time: u64,
        end: ProcessEnd,
    },
    /// Events the engine lost, on one CPU or thread, since the last such
    /// report. Every count the trace shows is a lower bound once one has
    /// occurred.
    Lost {
        time: u64,
        count: u64,
    },
}

impl TraceEvent {
    #[must_use]
    pub fn time(&self) -> u64 {
        match self {
            Self::Call(call) => call.time(),
            Self::Signal { time, .. }
            | Self::Spawn { time, .. }
            | Self::Exec { time, .. }
            | Self::Exit { time, .. }
            | Self::Lost { time, .. } => *time,
        }
    }
}

/// A call a thread is blocked in right now, read without waiting for it to
/// return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    pub thread: u32,
    pub process: u32,
    pub call: SyscallId,
    pub args: [u64; 6],
    /// `CLOCK_MONOTONIC` nanoseconds at entry.
    pub entered: u64,
}
