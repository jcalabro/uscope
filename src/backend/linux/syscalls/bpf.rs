//! The BPF engine for `uscope syscalls`, which never stops the traced
//! program.
//!
//! Programs in the kernel filter by process, time each chosen call, copy
//! the bytes behind its arguments, and send one record per call through a
//! ring buffer.
//!
//! The programs (`bpf/*.bpf.c`) are built by `build.rs` and embedded.
//! Records and map values cross the boundary as the plain structures of
//! `bpf/records.h`, which this module mirrors field for field; a test
//! compares both layouts through the object's BTF. Decoding a record is
//! pure. Everything else, from naming a call to printing it, is shared
//! with the ptrace engine.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aya::maps::{Array, HashMap, MapData, PerCpuArray, PerCpuHashMap, RingBuf};
use aya::programs::{BtfTracePoint, Iter};
use aya::{Btf, Ebpf, EbpfLoader};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::super::perf;
use super::Sink;
use super::launch::{self, Gate, GateOptions};
use super::ptrace::{Finished, Target, TraceOptions, monotonic, seed_descriptors};
use crate::syscalls::captures::{self, CopyKind};
use crate::syscalls::specs::{self, Arg, Count, MemKind, Ret, Struct};
use crate::syscalls::stats::{self, BUCKETS, Folded};
use crate::syscalls::{
    Abi, CallStack, Capture, CaptureState, Device, EventFlags, InFlight, Outcome, ProcessEnd,
    ReadAt, SyscallEvent, SyscallId, SyscallStats, TraceEvent,
};
use crate::unwind::sample::SampledState;

/// The programs, as `build.rs` compiled them.
static OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/syscalls.bpf.o"));

/// The tracepoints the programs attach to, by program name.
const PROGRAMS: [(&str, &str); 6] = [
    ("sys_enter", "sys_enter"),
    ("sys_exit", "sys_exit"),
    ("process_fork", "sched_process_fork"),
    ("process_exec", "sched_process_exec"),
    ("process_exit", "sched_process_exit"),
    ("signal_deliver", "signal_deliver"),
];

const RECORD_CALL: u32 = 1;
const RECORD_ENTRY: u32 = 2;
const RECORD_SPAWN: u32 = 3;
const RECORD_EXEC: u32 = 4;
const RECORD_EXIT: u32 = 5;
const RECORD_SIGNAL: u32 = 6;
const RECORD_FILE: u32 = 7;
const RECORD_STACK: u32 = 8;

const MAKES_RESULT: u8 = 1;
const MAKES_PAIR: u8 = 2;

const PIPEFS_MAGIC: u64 = 0x5049_5045;
const SOCKFS_MAGIC: u64 = 0x534f_434b;
const ANON_INODE_FS_MAGIC: u64 = 0x0904_1934;
const S_IFMT: u32 = 0o170_000;
const S_IFCHR: u32 = 0o020_000;
const S_IFBLK: u32 = 0o060_000;

const CONFIG_STREAM: u32 = 1;
const CONFIG_COUNT: u32 = 2;
const CONFIG_FOLLOW: u32 = 4;
const CONFIG_SKIP_UNSEEN: u32 = 8;
const CONFIG_STACKS: u32 = 16;
const CONFIG_STACK_COPY: u32 = 32;

const CALL_PROCESS_EXITED: u8 = 8;
/// The flags a call record shares with [`EventFlags`].
const CALL_EVENT_FLAGS: u8 = 2 | 4;

/// A call's slot in the CALLS map: its ABI and number.
const SLOTS: u32 = 3072;

/// The ring's default size, which a 3 M calls per second burst fills in
/// no less than a few milliseconds.
pub const DEFAULT_RING: u32 = 16 << 20;

/// How one argument is copied (`struct arg_copy`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawArgCopy {
    pub kind: u8,
    pub when: u8,
    pub writes: u8,
    pub count: u8,
    pub count_arg: u8,
    pub pad: [u8; 3],
    pub size: u32,
    pub limit: u32,
}

/// One call's slot in the CALLS map (`struct call_spec`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawCallSpec {
    pub selected: u8,
    pub moves: u8,
    pub makes: u8,
    pub makes_arg: u8,
    pub forks: u8,
    pub pad: u8,
    pub stat: u16,
    pub args: [RawArgCopy; 6],
}

/// `struct config`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawConfig {
    pub flags: u32,
    pub elements: u32,
    pub pad: u64,
}

/// A thread's call in progress (`struct entry`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawEntry {
    pub entered: u64,
    pub args: [u64; 6],
    pub nr: u32,
    pub tgid: u32,
    pub abi: u8,
    pub flags: u8,
    pub pad: [u8; 6],
}

/// `struct in_flight_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawInFlightRecord {
    pub tid: u32,
    pub pad: u32,
    pub entry: RawEntry,
}

/// Every record's start (`struct record`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawRecord {
    pub kind: u32,
    pub size: u32,
    pub tid: u32,
    pub tgid: u32,
}

/// `struct call_record`, which its captures follow.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawCallRecord {
    pub header: RawRecord,
    pub entered: u64,
    pub exited: u64,
    pub ret: i64,
    pub args: [u64; 6],
    pub nr: u32,
    pub abi: u8,
    pub flags: u8,
    pub captures: u16,
}

/// `struct capture_header`, which its bytes follow, padded to eight.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawCaptureHeader {
    pub arg: u8,
    pub when: u8,
    pub state: u8,
    pub pad: u8,
    pub len: u32,
    pub wanted: u32,
    pub pad2: u32,
}

/// `struct spawn_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawSpawnRecord {
    pub header: RawRecord,
    pub time: u64,
    pub child: u32,
    pub child_tgid: u32,
}

/// `struct exec_record`, which the new image's path follows.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawExecRecord {
    pub header: RawRecord,
    pub time: u64,
    pub former: u32,
    pub len: u32,
}

/// `struct exit_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawExitRecord {
    pub header: RawRecord,
    pub time: u64,
    pub code: i32,
    pub last: u32,
}

/// `struct signal_record`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawSignalRecord {
    pub header: RawRecord,
    pub time: u64,
    pub signal: i32,
    pub pad: u32,
    pub info: [u8; 48],
}

/// `struct file_record`, which the dentry's name follows.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawFileRecord {
    pub header: RawRecord,
    pub fd: i32,
    pub mode: u32,
    pub ino: u64,
    pub magic: u64,
    pub rdev: u32,
    pub len: u32,
    pub cloexec: u32,
    pub pad: u32,
}

/// `struct stack_record`, which the kernel's walk and the stack's copy
/// follow.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawStackRecord {
    pub header: RawRecord,
    pub frames: u32,
    pub copied: u32,
    pub regs: [u64; 17],
}

/// `struct stat_value`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawStatValue {
    pub calls: u64,
    pub errors: u64,
    pub timed: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub bytes: u64,
    pub log2_ns: [u64; BUCKETS],
}

/// `struct errno_key`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawErrnoKey {
    pub slot: u32,
    pub err: u32,
}

/// Map values cross as bytes, which aya can hold without unsafe code.
type Bytes<const N: usize> = [u8; N];
const SPEC_BYTES: usize = size_of::<RawCallSpec>();
const STAT_BYTES: usize = size_of::<RawStatValue>();
const ERRNO_KEY_BYTES: usize = size_of::<RawErrnoKey>();
const CONFIG_BYTES: usize = size_of::<RawConfig>();

const fn abi_of(code: u8) -> Option<Abi> {
    match code {
        0 => Some(Abi::X86_64),
        1 => Some(Abi::X86_32),
        2 => Some(Abi::X32),
        _ => None,
    }
}

/// A call's slot, as the programs key it: its ABI above ten bits of
/// number.
#[must_use]
pub const fn errno_slot(call: SyscallId) -> u32 {
    let abi = match call.abi {
        Abi::X86_64 => 0,
        Abi::X86_32 => 1,
        Abi::X32 => 2,
    };
    abi << 10 | call.number
}

/// The record the stream program writes for a call it read nothing of.
///
/// The call returned `returned`, or ended its process first, and was made
/// at the first time and returned at the second. A model of the stream
/// feeds such records to the decoder.
#[must_use]
pub fn encode_call(
    thread: u32,
    process: u32,
    call: SyscallId,
    args: [u64; 6],
    returned: Option<i64>,
    (entered, exited): (u64, u64),
) -> Vec<u8> {
    let record = RawCallRecord {
        header: RawRecord {
            kind: RECORD_CALL,
            size: u32::try_from(size_of::<RawCallRecord>()).unwrap_or(u32::MAX),
            tid: thread,
            tgid: process,
        },
        entered,
        exited,
        ret: returned.unwrap_or(0),
        args,
        nr: call.number,
        abi: u8::try_from(errno_slot(call) >> 10).unwrap_or(u8::MAX),
        flags: if returned.is_some() {
            0
        } else {
            CALL_PROCESS_EXITED
        },
        captures: 0,
    };
    record.as_bytes().to_vec()
}

/// The call a CALLS slot holds: its ABI above ten bits of number.
fn call_of(slot: u32) -> Option<SyscallId> {
    let abi = abi_of(u8::try_from(slot >> 10).ok()?)?;
    Some(SyscallId::new(abi, slot & 1023))
}

/// A call's CALLS slot value: whether it is chosen, and how to copy each
/// argument, from the rules the ptrace engine follows.
#[must_use]
pub fn call_spec(call: SyscallId, options: &TraceOptions) -> RawCallSpec {
    let mut spec = RawCallSpec {
        selected: 1,
        moves: u8::from(stats::moves(call)),
        forks: u8::from(matches!(
            call.name(),
            Some("clone" | "clone3" | "fork" | "vfork")
        )),
        ..RawCallSpec::default()
    };
    if let Some(described) = specs::spec(call) {
        if described.ret == Ret::Fd {
            spec.makes = MAKES_RESULT;
        }
        let pair = described.args.iter().position(|arg| {
            matches!(
                arg,
                Arg::Mem {
                    kind: MemKind::Struct(Struct::Fds2),
                    ..
                }
            )
        });
        if let Some(arg) = pair.and_then(|arg| u8::try_from(arg).ok()) {
            spec.makes = MAKES_PAIR;
            spec.makes_arg = arg;
        }
    }
    for copy in captures::copies(call, options.limits, options.verbose) {
        let (count, count_arg) = match copy.count {
            Count::One => (0, 0),
            Count::Arg(n) => (1, n),
            Count::Return => (2, 0),
            Count::PointedBy(n) => (3, n),
        };
        spec.args[usize::from(copy.arg)] = RawArgCopy {
            kind: match copy.kind {
                CopyKind::Path => 1,
                CopyKind::String => 2,
                CopyKind::Strings => 3,
                CopyKind::StringsCounted => 4,
                CopyKind::Bytes => 5,
                CopyKind::Iovec => 6,
                CopyKind::Sockaddr => 7,
                CopyKind::Struct => 8,
                CopyKind::FdSet => 9,
            },
            // A tracepoint cannot fault pages in, so what a call reads
            // is copied once the call has, at its exit.
            when: u8::from(copy.when == ReadAt::Exit || !reads_at_entry(call)),
            writes: u8::from(copy.writes),
            count,
            count_arg,
            pad: [0; 3],
            size: copy.size,
            limit: copy.limit,
        };
    }
    spec
}

/// Whether a call's input must be read at its entry: `execve` leaves
/// nothing of the old image to read at its exit, and `rseq` registers an
/// area the kernel writes before the call returns.
fn reads_at_entry(call: SyscallId) -> bool {
    matches!(call.name(), Some("execve" | "execveat" | "rseq"))
}

/// One record from the ring, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A call, at its exit, or as its thread ended inside it.
    Call(SyscallEvent),
    /// The arguments a call read at its entry; its exit follows.
    Entry(SyscallEvent),
    Event(TraceEvent),
    /// The stack of the thread whose call or signal follows.
    Stack(SampledStack),
}

/// A thread's stack as the kernel saw it, to unwind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledStack {
    pub thread: u32,
    pub process: u32,
    /// The user registers by DWARF number.
    pub registers: [(u16, u64); 17],
    /// The kernel's walk of the frame pointers, the instruction first.
    pub chain: Vec<u64>,
    /// The stack's bytes from its pointer up, if copied.
    pub copy: Vec<u8>,
}

/// Why a record could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("a record of {0} bytes is shorter than its header says")]
    Short(usize),
    #[error("unknown record kind {0}")]
    Kind(u32),
    #[error("a call record names ABI {0}")]
    Abi(u8),
    #[error("a capture of argument {0} is malformed")]
    Capture(u8),
}

fn read<T: FromBytes + KnownLayout + Immutable>(bytes: &[u8]) -> Result<T, DecodeError> {
    T::read_from_prefix(bytes)
        .map(|(value, _)| value)
        .map_err(|_| DecodeError::Short(bytes.len()))
}

/// How a process ended, from the exit code `wait(2)` encodes.
#[must_use]
pub const fn process_end(code: i32) -> ProcessEnd {
    let signal = code & 0x7f;
    if signal == 0 {
        ProcessEnd::Exited((code >> 8) & 0xff)
    } else {
        ProcessEnd::Killed {
            signal,
            core: code & 0x80 != 0,
        }
    }
}

/// Decodes one record as the programs wrote it.
///
/// # Errors
///
/// A record that is shorter than its header says, of an unknown kind, or
/// whose captures overrun it.
pub fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
    let header: RawRecord = read(bytes)?;
    let size = usize::try_from(header.size).unwrap_or(usize::MAX);
    let bytes = bytes.get(..size).ok_or(DecodeError::Short(bytes.len()))?;
    match header.kind {
        RECORD_CALL | RECORD_ENTRY => call_record(header, bytes),
        RECORD_SPAWN => {
            let raw: RawSpawnRecord = read(bytes)?;
            Ok(Record::Event(TraceEvent::Spawn {
                parent_thread: header.tid,
                parent: header.tgid,
                child: raw.child,
                thread: raw.child_tgid == header.tgid,
                time: raw.time,
            }))
        }
        RECORD_EXEC => {
            let raw: RawExecRecord = read(bytes)?;
            let start = size_of::<RawExecRecord>();
            let path = bytes
                .get(start..start + raw.len as usize)
                .ok_or(DecodeError::Short(bytes.len()))?;
            Ok(Record::Event(TraceEvent::Exec {
                thread: header.tid,
                process: header.tgid,
                time: raw.time,
                path: String::from_utf8_lossy(path).into(),
            }))
        }
        RECORD_EXIT => {
            let raw: RawExitRecord = read(bytes)?;
            Ok(Record::Event(if raw.last == 0 {
                TraceEvent::ThreadExit {
                    thread: header.tid,
                    process: header.tgid,
                    time: raw.time,
                    end: process_end(raw.code),
                }
            } else {
                TraceEvent::Exit {
                    process: header.tgid,
                    time: raw.time,
                    end: process_end(raw.code),
                }
            }))
        }
        RECORD_SIGNAL => {
            let raw: RawSignalRecord = read(bytes)?;
            let word =
                |at: usize| i32::from_le_bytes(raw.info[at..at + 4].try_into().unwrap_or_default());
            let code = word(8);
            // Processes send with SI_USER, SI_QUEUE, SI_TKILL, and the like.
            let sender = (code <= 0 && code != -2)
                .then(|| u32::try_from(word(16)).ok())
                .flatten();
            // The rest of a siginfo holds no field of any kind of signal.
            let mut info = [0_u8; 128];
            info[..raw.info.len()].copy_from_slice(&raw.info);
            Ok(Record::Event(TraceEvent::Signal {
                thread: header.tid,
                process: header.tgid,
                time: raw.time,
                signal: raw.signal,
                code: Some(code),
                sender,
                info: Some(Arc::from(&info[..])),
                stack: None,
            }))
        }
        RECORD_STACK => stack_record(header, bytes),
        RECORD_FILE => {
            let raw: RawFileRecord = read(bytes)?;
            let start = size_of::<RawFileRecord>();
            let name = bytes
                .get(start..start + raw.len as usize)
                .ok_or(DecodeError::Short(bytes.len()))?;
            Ok(Record::Event(descriptor(header.tgid, &raw, name)))
        }
        kind => Err(DecodeError::Kind(kind)),
    }
}

/// A stack record and the walk and copy that follow it.
fn stack_record(header: RawRecord, bytes: &[u8]) -> Result<Record, DecodeError> {
    let raw: RawStackRecord = read(bytes)?;
    let start = size_of::<RawStackRecord>();
    let chain_end = start + raw.frames as usize * 8;
    let chain = bytes
        .get(start..chain_end)
        .ok_or(DecodeError::Short(bytes.len()))?;
    let copy = bytes
        .get(chain_end..chain_end + raw.copied as usize)
        .ok_or(DecodeError::Short(bytes.len()))?;
    #[expect(clippy::cast_possible_truncation, reason = "seventeen registers")]
    let registers = std::array::from_fn(|index| (index as u16, raw.regs[index]));
    Ok(Record::Stack(SampledStack {
        thread: header.tid,
        process: header.tgid,
        registers,
        chain: chain
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect(),
        copy: copy.to_vec(),
    }))
}

/// A call record and the captures that follow it.
fn call_record(header: RawRecord, bytes: &[u8]) -> Result<Record, DecodeError> {
    let raw: RawCallRecord = read(bytes)?;
    let abi = abi_of(raw.abi).ok_or(DecodeError::Abi(raw.abi))?;
    let mut captured = Vec::with_capacity(usize::from(raw.captures));
    let mut at = size_of::<RawCallRecord>();
    for _ in 0..raw.captures {
        let capture: RawCaptureHeader =
            read(bytes.get(at..).ok_or(DecodeError::Short(bytes.len()))?)?;
        let start = at + size_of::<RawCaptureHeader>();
        let len = capture.len as usize;
        let data = bytes
            .get(start..start + len)
            .ok_or(DecodeError::Capture(capture.arg))?;
        captured.push(Capture {
            arg: capture.arg,
            when: if capture.when == 0 {
                ReadAt::Entry
            } else {
                ReadAt::Exit
            },
            state: match capture.state {
                0 => CaptureState::Complete,
                1 => CaptureState::Truncated {
                    wanted: capture.wanted,
                },
                2 => CaptureState::Unreadable,
                _ => return Err(DecodeError::Capture(capture.arg)),
            },
            bytes: Arc::from(data),
        });
        at = start + len.next_multiple_of(8);
    }
    let unseen = raw.flags & 2 != 0;
    let exited_inside = raw.flags & CALL_PROCESS_EXITED != 0;
    let event = SyscallEvent {
        thread: header.tid,
        process: header.tgid,
        call: SyscallId::new(abi, raw.nr),
        args: raw.args,
        captured,
        outcome: if header.kind == RECORD_ENTRY {
            Outcome::Unfinished
        } else if exited_inside {
            Outcome::ProcessExited
        } else {
            Outcome::of_return(raw.ret)
        },
        entered: (!unseen).then_some(raw.entered),
        exited: (header.kind == RECORD_CALL && !exited_inside).then_some(raw.exited),
        stack: None,
        flags: EventFlags(raw.flags & CALL_EVENT_FLAGS),
    };
    Ok(if header.kind == RECORD_ENTRY {
        Record::Entry(event)
    } else {
        Record::Call(event)
    })
}

/// What a descriptor a call made refers to, as `/proc/PID/fd/N` would
/// name it. The kernel gives no path, so a file's text is left empty and
/// the descriptor table keeps the path the call named.
fn descriptor(process: u32, raw: &RawFileRecord, name: &[u8]) -> TraceEvent {
    let name = String::from_utf8_lossy(name);
    let text = match raw.magic {
        PIPEFS_MAGIC => format!("pipe:[{}]", raw.ino),
        SOCKFS_MAGIC => format!("socket:[{}]", raw.ino),
        ANON_INODE_FS_MAGIC => format!("anon_inode:{name}"),
        _ => String::new(),
    };
    let kind = raw.mode & S_IFMT;
    let device = (kind == S_IFCHR || kind == S_IFBLK).then_some(Device {
        block: kind == S_IFBLK,
        major: raw.rdev >> 20,
        minor: raw.rdev & 0xf_ffff,
    });
    TraceEvent::Descriptor {
        process,
        fd: i64::from(raw.fd),
        time: 0,
        text: text.into(),
        cloexec: raw.cloexec != 0,
        device,
        made: true,
    }
}

/// Folds the kernel's per-CPU statistics into one per call, which the
/// kernel counts over every traced process and `Folded` names by `process`.
#[must_use]
pub fn fold(
    process: u32,
    stats: impl IntoIterator<Item = (SyscallId, Vec<RawStatValue>)>,
    errnos: impl IntoIterator<Item = (RawErrnoKey, u64)>,
) -> Folded {
    let mut folded = Folded::default();
    for (call, per_cpu) in stats {
        let into = folded.by_call.entry((process, call)).or_default();
        for value in per_cpu {
            into.merge(&SyscallStats {
                calls: value.calls,
                errors: value.errors,
                errors_by_errno: BTreeMap::new(),
                timed: value.timed,
                total_ns: value.total_ns,
                min_ns: value.min_ns,
                max_ns: value.max_ns,
                log2_ns: value.log2_ns.to_vec(),
                bytes: value.bytes,
            });
        }
    }
    for (key, count) in errnos {
        let (Some(call), Ok(errno)) = (call_of(key.slot), u16::try_from(key.err)) else {
            continue;
        };
        if count > 0 {
            *folded
                .by_call
                .entry((process, call))
                .or_default()
                .errors_by_errno
                .entry(errno)
                .or_default() += count;
        }
    }
    folded
}

/// Why the BPF engine cannot trace here, each with what to do about it.
#[derive(Debug, thiserror::Error)]
pub enum BpfError {
    #[error("{0}")]
    Unavailable(String),
    #[error("starting the program failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("process {0} does not exist")]
    NoProcess(u32),
    #[error("loading the BPF programs failed: {0}")]
    Load(String),
    #[error("tracing failed: {0}")]
    System(#[from] nix::errno::Errno),
    #[error("following what the program maps, for its stacks, failed: {0}")]
    Mappings(#[source] super::super::perf::OpenError),
}

/// What to do if the engine is missing a privilege.
const REMEDY: &str = "  uscope uses eBPF so it never stops or slows the traced program, and it will
  not fall back to ptrace on its own. To continue, one of:
    - run it from a unit with AmbientCapabilities=CAP_BPF CAP_PERFMON
    - on NixOS or system-manager, import uscope's bpf module and run through
      /run/wrappers/bin/uscope-bpf (docs/syscalls.md#privileges)
    - trace a program you launch with --engine ptrace, which stops the
      program at each chosen call";

const CAP_SYS_ADMIN: u32 = 21;
const CAP_PERFMON: u32 = 38;
const CAP_BPF: u32 = 39;

/// What the running kernel and this process offer the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The kernel's release, as `uname -r` prints it.
    pub release: String,
    /// Its major and minor version.
    pub version: Option<(u32, u32)>,
    pub btf: bool,
    /// The effective capability set.
    pub capabilities: u64,
    /// The lockdown mode in force, when the kernel has one.
    pub lockdown: Option<String>,
}

impl Probe {
    /// Reads what the system offers.
    #[must_use]
    pub fn read() -> Self {
        let release = nix::sys::utsname::uname()
            .map(|name| name.release().to_string_lossy().into_owned())
            .unwrap_or_default();
        let capabilities = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("CapEff:"))
                    .and_then(|bits| u64::from_str_radix(bits.trim(), 16).ok())
            })
            .unwrap_or(0);
        let lockdown = std::fs::read_to_string("/sys/kernel/security/lockdown")
            .ok()
            .and_then(|modes| {
                let start = modes.find('[')?;
                let end = modes[start..].find(']')?;
                Some(modes[start + 1..start + end].to_owned())
            });
        Self {
            version: version(&release),
            release,
            btf: std::path::Path::new("/sys/kernel/btf/vmlinux").exists(),
            capabilities,
            lockdown,
        }
    }

    const fn has(&self, capability: u32) -> bool {
        self.capabilities & (1 << capability) != 0
    }

    /// Whether the engine can run, or the one specific reason it cannot,
    /// with the fix.
    ///
    /// # Errors
    ///
    /// The first missing piece, in the order a reader fixes them.
    pub fn check(&self) -> Result<(), BpfError> {
        if self.version.is_none_or(|version| version < (5, 15)) {
            return Err(BpfError::Unavailable(format!(
                "uscope syscalls' BPF engine needs Linux 5.15 or newer, and this kernel is {}.\n  \
                 Trace a program you launch with --engine ptrace instead.",
                self.release
            )));
        }
        if !self.btf {
            return Err(BpfError::Unavailable(
                "uscope syscalls' BPF engine needs the kernel's BTF, and /sys/kernel/btf/vmlinux \
                 is missing.\n  Build the kernel with CONFIG_DEBUG_INFO_BTF=y, or trace a program \
                 you launch with --engine ptrace."
                    .to_owned(),
            ));
        }
        let bpf = self.has(CAP_BPF) || self.has(CAP_SYS_ADMIN);
        let perfmon = self.has(CAP_PERFMON) || self.has(CAP_SYS_ADMIN);
        let missing = match (bpf, perfmon) {
            (true, true) => None,
            (false, false) => Some("has neither"),
            (false, true) => Some("lacks CAP_BPF"),
            (true, false) => Some("lacks CAP_PERFMON"),
        };
        if let Some(missing) = missing {
            return Err(BpfError::Unavailable(format!(
                "uscope syscalls needs CAP_BPF and CAP_PERFMON, and this process {missing}.\n{REMEDY}"
            )));
        }
        if self.lockdown.as_deref() == Some("confidentiality") {
            return Err(BpfError::Unavailable(
                "the kernel is locked down for confidentiality, which forbids BPF programs to \
                 read kernel memory.\n  Boot with lockdown=integrity or none, or trace a program \
                 you launch with --engine ptrace."
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

fn version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// How the BPF engine runs, beyond what to trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BpfOptions {
    /// Count in the kernel and report no calls, only their statistics.
    pub count_only: bool,
    /// The ring buffer's bytes: a power of two, in pages.
    pub ring: u32,
    /// With stacks, copy each stack's top to unwind by call-frame
    /// information, rather than trust the kernel's walk alone.
    pub stack_copy: bool,
}

impl Default for BpfOptions {
    fn default() -> Self {
        Self {
            count_only: false,
            ring: DEFAULT_RING,
            stack_copy: false,
        }
    }
}

fn load_error(error: impl std::fmt::Display) -> BpfError {
    BpfError::Load(error.to_string())
}

/// The loaded and attached programs, and their maps.
struct Loaded {
    ebpf: Ebpf,
    targets: HashMap<MapData, u32, u8>,
    /// The call each row of statistics counts.
    rows: Vec<SyscallId>,
}

impl Loaded {
    fn load(options: &TraceOptions, engine: BpfOptions, launch: bool) -> Result<Self, BpfError> {
        // Every slot when every call is chosen, numbers no table names
        // among them. Each chosen call has a row of statistics.
        let all = options.calls.is_all();
        let chosen: Vec<(u32, SyscallId)> = (0..SLOTS)
            .filter_map(|slot| Some((slot, call_of(slot)?)))
            .filter(|&(_, call)| all || options.calls.contains(call))
            .collect();
        let rows = u32::try_from(chosen.len().max(1)).unwrap_or(u32::MAX);
        let btf = Btf::from_sys_fs().map_err(load_error)?;
        let mut ebpf = EbpfLoader::new()
            .btf(Some(&btf))
            .map_max_entries("EVENTS", engine.ring)
            .map_max_entries("STATS", rows)
            // aya cannot open task storage itself, which only the
            // programs use.
            .allow_unsupported_maps()
            .load(OBJECT)
            .map_err(load_error)?;
        {
            let mut calls: Array<_, Bytes<SPEC_BYTES>> =
                Array::try_from(ebpf.map_mut("CALLS").ok_or_else(|| missing("CALLS"))?)
                    .map_err(load_error)?;
            for (row, &(slot, call)) in chosen.iter().enumerate() {
                let mut spec = call_spec(call, options);
                spec.stat = u16::try_from(row).unwrap_or(u16::MAX);
                let mut value = [0; SPEC_BYTES];
                value.copy_from_slice(spec.as_bytes());
                calls.set(slot, value, 0).map_err(load_error)?;
            }
        }
        {
            let mut config: Array<_, Bytes<CONFIG_BYTES>> =
                Array::try_from(ebpf.map_mut("CONFIG").ok_or_else(|| missing("CONFIG"))?)
                    .map_err(load_error)?;
            let mut flags = if engine.count_only {
                CONFIG_COUNT
            } else {
                CONFIG_STREAM
            };
            if options.follow {
                flags |= CONFIG_FOLLOW;
            }
            if launch {
                flags |= CONFIG_SKIP_UNSEEN;
            }
            if options.stacks.is_some() {
                flags |= CONFIG_STACKS;
                if engine.stack_copy {
                    flags |= CONFIG_STACK_COPY;
                }
            }
            let raw = RawConfig {
                flags,
                elements: u32::try_from(options.limits.elements).unwrap_or(u32::MAX),
                pad: 0,
            };
            let mut value = [0; CONFIG_BYTES];
            value.copy_from_slice(raw.as_bytes());
            config.set(0, value, 0).map_err(load_error)?;
        }
        for (name, tracepoint) in PROGRAMS {
            let program: &mut BtfTracePoint = ebpf
                .program_mut(name)
                .ok_or_else(|| missing(name))?
                .try_into()
                .map_err(load_error)?;
            program.load(tracepoint, &btf).map_err(load_error)?;
            program.attach().map_err(load_error)?;
        }
        let iterator: &mut Iter = ebpf
            .program_mut("in_flight")
            .ok_or_else(|| missing("in_flight"))?
            .try_into()
            .map_err(load_error)?;
        iterator.load("task", &btf).map_err(load_error)?;
        let targets =
            HashMap::try_from(ebpf.take_map("TARGETS").ok_or_else(|| missing("TARGETS"))?)
                .map_err(load_error)?;
        record!("syscalls bpf: loaded and attached");
        Ok(Self {
            ebpf,
            targets,
            rows: chosen.into_iter().map(|(_, call)| call).collect(),
        })
    }

    fn target(&mut self, process: u32) -> Result<(), BpfError> {
        record!("syscalls bpf: trace {process}");
        self.targets.insert(process, 1, 0).map_err(load_error)
    }

    /// Calls threads have been in for at least `after` nanoseconds, which
    /// the `in_flight` iterator lists from each thread's own storage.
    fn in_flight(&mut self, after: u64) -> Vec<InFlight> {
        let listed = match self.list_in_flight() {
            Ok(listed) => listed,
            // Only the flight recorder, which release builds leave out,
            // reads the error.
            #[cfg_attr(not(debug_assertions), expect(unused_variables))]
            Err(error) => {
                record!("syscalls bpf: the in-flight scan failed: {error}");
                Vec::new()
            }
        };
        let now = monotonic();
        let mut calls: Vec<InFlight> = listed
            .as_chunks::<{ size_of::<RawInFlightRecord>() }>()
            .0
            .iter()
            .filter_map(|bytes| {
                let record = RawInFlightRecord::read_from_bytes(bytes).ok()?;
                let entry = record.entry;
                (now.saturating_sub(entry.entered) >= after).then_some(InFlight {
                    thread: record.tid,
                    process: entry.tgid,
                    call: SyscallId::new(abi_of(entry.abi)?, entry.nr),
                    args: entry.args,
                    entered: entry.entered,
                })
            })
            .collect();
        calls.sort_by_key(|call| (call.entered, call.thread));
        calls
    }

    /// Runs the `in_flight` iterator once: the records it wrote.
    fn list_in_flight(&mut self) -> Result<Vec<u8>, BpfError> {
        use std::io::Read as _;
        let program: &mut Iter = self
            .ebpf
            .program_mut("in_flight")
            .ok_or_else(|| missing("in_flight"))?
            .try_into()
            .map_err(load_error)?;
        let link = program.attach().map_err(load_error)?;
        let link = program.take_link(link).map_err(load_error)?;
        let mut file = link.into_file().map_err(load_error)?;
        let mut listed = Vec::new();
        file.read_to_end(&mut listed).map_err(load_error)?;
        Ok(listed)
    }

    /// Calls in progress as the trace ends, which never returned while it
    /// ran.
    fn unfinished(&mut self) -> Vec<SyscallEvent> {
        self.in_flight(0)
            .into_iter()
            .map(|call| SyscallEvent {
                thread: call.thread,
                process: call.process,
                call: call.call,
                args: call.args,
                captured: Vec::new(),
                outcome: Outcome::Unfinished,
                entered: Some(call.entered),
                exited: None,
                stack: None,
                flags: EventFlags::default(),
            })
            .collect()
    }

    /// Records the ring had no room for, over every CPU.
    fn lost(&self) -> u64 {
        let Some(map) = self.ebpf.map("LOST") else {
            return 0;
        };
        PerCpuArray::<_, u64>::try_from(map)
            .ok()
            .and_then(|lost| lost.get(&0, 0).ok())
            .map_or(0, |values| values.iter().sum())
    }

    fn counted(&self, process: u32) -> Folded {
        let stats = self
            .ebpf
            .map("STATS")
            .and_then(|map| PerCpuArray::<_, Bytes<STAT_BYTES>>::try_from(map).ok())
            .map(|map| {
                (0_u32..)
                    .zip(&self.rows)
                    .filter_map(|(row, &call)| {
                        let values = map
                            .get(&row, 0)
                            .ok()?
                            .iter()
                            .filter_map(|value| RawStatValue::read_from_bytes(value).ok())
                            .collect::<Vec<_>>();
                        values
                            .iter()
                            .any(|value| value.calls > 0)
                            .then_some((call, values))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let errnos = self
            .ebpf
            .map("ERRNOS")
            .and_then(|map| PerCpuHashMap::<_, Bytes<ERRNO_KEY_BYTES>, u64>::try_from(map).ok())
            .map(|map| {
                map.iter()
                    .flatten()
                    .filter_map(|(key, values)| {
                        Some((
                            RawErrnoKey::read_from_bytes(&key).ok()?,
                            values.iter().sum::<u64>(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        fold(process, stats, errnos)
    }
}

fn missing(name: &str) -> BpfError {
    BpfError::Load(format!("the BPF object has no {name}"))
}

/// Whether a process has ended: gone, or a zombie.
fn ended(process: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{process}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X'))
    })
}

/// Traces `target` until every traced process ends, or until `stop` is set,
/// which kills a launched program and leaves attached ones running. Each
/// event goes to `sink` as it arrives.
///
/// # Errors
///
/// What the probe found missing, a failure to load or attach the
/// programs, or to start the program.
pub fn trace(
    target: Target,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    let start = match target {
        Target::Launch(command) => Start::Launch(command),
        Target::Attach(pids) => Start::Attach(pids),
    };
    run(start, options, engine, stop, sink)
}

/// Traces process `pid`, held at another launcher's gate, from its first
/// call.
///
/// It is traced as [`trace`] traces a program it launches. The sink hears
/// [`Sink::attached`] once the gate may open.
///
/// # Errors
///
/// As [`trace`]'s.
pub fn trace_held(
    pid: u32,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    run(Start::Held(pid), options, engine, stop, sink)
}

/// How a trace finds its first process.
enum Start {
    Launch(std::process::Command),
    Attach(Vec<u32>),
    Held(u32),
}

fn run(
    start: Start,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    Probe::read().check()?;
    let mut loaded = Loaded::load(options, engine, !matches!(start, Start::Attach(_)))?;
    let mut gate: Option<Gate> = None;
    let mut live = BTreeSet::new();
    let mut mappings = Vec::new();
    let first = match start {
        Start::Held(pid) => {
            loaded.target(pid)?;
            if options.stacks.is_some() {
                mappings.push(perf::Events::open(pid, &MAPPINGS).map_err(BpfError::Mappings)?);
            }
            seed_descriptors(pid, sink);
            live.insert(pid);
            sink.attached(pid);
            pid
        }
        Start::Launch(command) => {
            let mut launched = launch::launch(
                command,
                GateOptions {
                    filter: None,
                    reap: true,
                },
            )
            .map_err(BpfError::Spawn)?;
            let pid = launched.pid;
            loaded.target(pid)?;
            if options.stacks.is_some() {
                // Enabled as it executes, as the profiler's are.
                mappings.push(perf::Events::open(pid, &MAPPINGS).map_err(BpfError::Mappings)?);
            }
            // Held at its gate, it has the descriptors it will execute with.
            seed_descriptors(pid, sink);
            live.insert(pid);
            launched.open().map_err(BpfError::Spawn)?;
            gate = Some(launched);
            pid
        }
        Start::Attach(pids) => {
            for &pid in &pids {
                if ended(pid) {
                    return Err(BpfError::NoProcess(pid));
                }
                loaded.target(pid)?;
                if let Some(stacks) = &options.stacks {
                    mappings.push(
                        perf::Events::attach(pid, &MAPPINGS, MAPPING_EVENTS)
                            .map_err(BpfError::Mappings)?,
                    );
                    // What it mapped before, while it is there to read.
                    stacks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .read_maps(pid);
                }
                live.insert(pid);
                sink.attached(pid);
            }
            pids.first().copied().unwrap_or(0)
        }
    };
    let mut reader = Reader {
        sink,
        follow: options.follow,
        live,
        first,
        end: None,
        entries: BTreeMap::new(),
        stacks: options.stacks.clone(),
        sampled: BTreeMap::new(),
        mappings,
        batch: Vec::new(),
        perf: Vec::new(),
        lost: 0,
    };
    let result = reader.run(&mut loaded, stop, gate.as_ref().map(|gate| gate.pid));
    let ring_end = reader.end;
    let Reader { sink, .. } = reader;
    if engine.count_only {
        sink.counted(loaded.counted(first));
    }
    drop(loaded);
    let status = gate.as_mut().map(Gate::finish);
    result?;
    // The launched program's status is the one its parent reaped.
    let end = match status {
        Some(Ok(Some(status))) => {
            use std::os::unix::process::ExitStatusExt as _;
            Some(status.code().map_or_else(
                || ProcessEnd::Killed {
                    signal: status.signal().unwrap_or(0),
                    core: status.core_dumped(),
                },
                ProcessEnd::Exited,
            ))
        }
        Some(Err(error)) if ring_end.is_none() => return Err(BpfError::Spawn(error)),
        _ => ring_end,
    };
    Ok(Finished { end })
}

/// Events that sample nothing and say what the traced processes map.
const MAPPINGS: perf::EventOptions = perf::EventOptions {
    source: perf::Source::Mappings,
    stack_bytes: 0,
    ring_pages: 64,
    switches: false,
};

/// The most such events an attach opens: one per thread and CPU.
const MAPPING_EVENTS: usize = 65_536;

/// Reads the ring and turns records into events.
struct Reader<'a> {
    sink: &'a mut dyn Sink,
    follow: bool,
    /// The traced processes still running.
    live: BTreeSet<u32>,
    first: u32,
    end: Option<ProcessEnd>,
    /// Arguments read at a call's entry, by thread, until its exit.
    entries: BTreeMap<u32, SyscallEvent>,
    /// Unwinds each stack, when stacks were asked for.
    stacks: Option<super::stacks::SharedStacks>,
    /// Each thread's stack, until the call or signal it precedes.
    sampled: BTreeMap<u32, SampledStack>,
    /// What the traced processes map, as they map it, for their stacks.
    mappings: Vec<perf::Events>,
    batch: Vec<Record>,
    perf: Vec<u8>,
    lost: u64,
}

/// How long the reader waits for the ring before it looks at the world.
const TICK_MS: u16 = 100;

impl Reader<'_> {
    fn run(
        &mut self,
        loaded: &mut Loaded,
        stop: &AtomicBool,
        launched: Option<u32>,
    ) -> Result<(), BpfError> {
        let mut ring = RingBuf::try_from(
            loaded
                .ebpf
                .take_map("EVENTS")
                .ok_or_else(|| missing("EVENTS"))?,
        )
        .map_err(load_error)?;
        let mut stopping = false;
        let mut last_tick = monotonic();
        loop {
            if stop.load(Ordering::Relaxed) && !stopping {
                stopping = true;
                record!("syscalls bpf: stop requested");
                match launched {
                    Some(pid) => {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid.cast_signed()),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                    // Attached processes run on untraced.
                    None => self.live.clear(),
                }
            }
            // Processes that ended without a record, as when the ring was
            // full, are seen in /proc; whatever they sent is in the ring
            // before they are.
            let gone: Vec<u32> = self
                .live
                .iter()
                .copied()
                .filter(|&pid| ended(pid))
                .collect();
            self.drain(&mut ring);
            for pid in gone {
                self.live.remove(&pid);
            }
            if stopping && launched.is_some_and(|pid| !self.live.contains(&pid)) {
                self.live.clear();
            }
            if self.live.is_empty() {
                self.drain(&mut ring);
                break;
            }
            let now = monotonic();
            if now - last_tick >= u64::from(TICK_MS) * 1_000_000 {
                last_tick = now;
                self.report_lost(loaded, now);
                if let Some(after) = self.sink.in_flight_after() {
                    self.sink.in_flight(&loaded.in_flight(after));
                }
            }
            let mut fds = [nix::poll::PollFd::new(
                ring.as_fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(TICK_MS)) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(error) => return Err(BpfError::System(error)),
            }
        }
        self.report_lost(loaded, monotonic());
        for event in loaded.unfinished() {
            self.sink.event(TraceEvent::Call(event));
        }
        for (_, entry) in std::mem::take(&mut self.entries) {
            self.sink.event(TraceEvent::Call(entry));
        }
        Ok(())
    }

    fn report_lost(&mut self, loaded: &Loaded, time: u64) {
        let lost = loaded.lost();
        if lost > self.lost {
            record!("syscalls bpf: {} records lost", lost - self.lost);
            self.sink.event(TraceEvent::Lost {
                time,
                count: lost - self.lost,
            });
            self.lost = lost;
        }
    }

    fn drain(&mut self, ring: &mut RingBuf<MapData>) {
        // A stack's code was mapped before the stack was sent, so the
        // mappings read after the records name all it needs.
        let mut batch = std::mem::take(&mut self.batch);
        while let Some(item) = ring.next() {
            match decode(&item) {
                Ok(record) => batch.push(record),
                // Only the flight recorder, which release builds leave
                // out, reads the error.
                #[cfg_attr(not(debug_assertions), expect(unused_variables))]
                Err(error) => {
                    record!("syscalls bpf: undecodable record: {error}");
                    self.sink.event(TraceEvent::Lost {
                        time: monotonic(),
                        count: 1,
                    });
                }
            }
        }
        self.map();
        for record in std::mem::take(&mut batch) {
            self.handle(record);
        }
        self.batch = batch;
    }

    /// Takes in what the traced processes mapped since last asked, in the
    /// order they mapped it.
    fn map(&mut self) {
        let Some(stacks) = &self.stacks else {
            return;
        };
        self.perf.clear();
        for events in &mut self.mappings {
            events.drain(&mut self.perf);
        }
        let mut records = Vec::new();
        let mut offset = 0;
        while let Some(Ok((record, length))) = perf::decode::next(&self.perf[offset..]) {
            records.push((record.time().unwrap_or(0), offset));
            offset += length;
        }
        records.sort_unstable();
        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, offset) in records {
            match perf::decode::next(&self.perf[offset..]) {
                Some(Ok((perf::decode::Record::Mmap(mmap), _))) if mmap.executable => {
                    stacks.mapped(&mmap);
                }
                Some(Ok((
                    perf::decode::Record::Comm {
                        pid, exec: true, ..
                    },
                    _,
                ))) => stacks.forget(pid),
                Some(Ok((perf::decode::Record::Fork { pid, ppid, .. }, _))) if pid != ppid => {
                    stacks.forked(ppid, pid);
                }
                _ => {}
            }
        }
    }

    /// Unwinds the stack the kernel sent before a thread's call or signal.
    fn unwind(&mut self, thread: u32) -> Option<CallStack> {
        let sampled = self.sampled.remove(&thread)?;
        let stacks = self.stacks.as_ref()?;
        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(stacks.unwind_sampled(
            sampled.process,
            SampledState {
                registers: &sampled.registers,
                stack: &sampled.copy,
                chain: &sampled.chain,
            },
        ))
    }

    fn handle(&mut self, record: Record) {
        match record {
            Record::Stack(stack) => {
                self.sampled.insert(stack.thread, stack);
            }
            Record::Entry(entry) => {
                self.entries.insert(entry.thread, entry);
            }
            Record::Call(mut call) => {
                call.stack = self.unwind(call.thread);
                if let Some(entry) = self.entries.remove(&call.thread) {
                    if Some(entry.entered.unwrap_or(0)) == call.entered.or(Some(0)) {
                        let mut captured = entry.captured;
                        captured.append(&mut call.captured);
                        captured.sort_by_key(|capture| capture.arg);
                        call.captured = captured;
                    } else {
                        // The entry's exit was never seen.
                        self.sink.event(TraceEvent::Call(entry));
                    }
                }
                self.sink.event(TraceEvent::Call(call));
            }
            Record::Event(mut event) => {
                // A file record precedes its call, whose exit it shares.
                if let TraceEvent::Descriptor { time, .. } = &mut event {
                    *time = monotonic();
                }
                if let TraceEvent::Signal { thread, stack, .. } = &mut event {
                    *stack = self.unwind(*thread);
                }
                match &event {
                    TraceEvent::Spawn { child, thread, .. } if !*thread && self.follow => {
                        self.live.insert(*child);
                    }
                    TraceEvent::Exec {
                        thread, process, ..
                    } if thread != process => {
                        // A thread that executes takes its process's id.
                        if let Some(mut entry) = self.entries.remove(thread) {
                            entry.thread = *process;
                            self.entries.insert(*process, entry);
                        }
                    }
                    TraceEvent::Exit { process, end, .. } => {
                        if *process == self.first {
                            self.end = Some(*end);
                        }
                        self.live.remove(process);
                    }
                    _ => {}
                }
                self.sink.event(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::captures::MOST_BYTES;

    /// The object's BTF: each struct's size and its members' names and
    /// byte offsets.
    fn btf_structs() -> BTreeMap<String, (u32, Vec<(String, u32)>)> {
        use object::{Object as _, ObjectSection as _};
        let file = object::File::parse(OBJECT).expect("the object parses");
        let section = file.section_by_name(".BTF").expect("a .BTF section");
        let data = section.data().expect("its bytes");
        let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().expect("4"));
        let header_len = u32_at(4) as usize;
        let (type_off, type_len) = (u32_at(8) as usize, u32_at(12) as usize);
        let str_off = u32_at(16) as usize;
        let types = header_len + type_off;
        let strings = header_len + str_off;
        let name = |offset: u32| {
            let start = strings + offset as usize;
            let end = start + data[start..].iter().position(|&b| b == 0).expect("NUL");
            String::from_utf8_lossy(&data[start..end]).into_owned()
        };
        let mut structs = BTreeMap::new();
        let mut at = types;
        while at < types + type_len {
            let (name_off, info, size) = (u32_at(at), u32_at(at + 4), u32_at(at + 8));
            let kind = (info >> 24) & 0x1f;
            let vlen = (info & 0xffff) as usize;
            at += 12;
            // Each kind's trailing data, by the BTF format.
            let trailing = match kind {
                1 | 14 | 15 | 17 => 4,
                3 => 12,
                4 | 5 => 12 * vlen,
                6 | 13 => 8 * vlen,
                19 => 16 * vlen,
                _ => 0,
            };
            if kind == 4 {
                let members = (0..vlen)
                    .map(|index| {
                        let member = at + index * 12;
                        (name(u32_at(member)), (u32_at(member + 8) & 0x00ff_ffff) / 8)
                    })
                    .collect();
                structs.insert(name(name_off), (size, members));
            }
            at += trailing;
        }
        structs
    }

    macro_rules! layout {
        ($type:ty, $c:literal, [$($field:ident),*]) => {
            (
                $c,
                size_of::<$type>(),
                vec![$((stringify!($field), std::mem::offset_of!($type, $field))),*],
            )
        };
    }

    #[test]
    fn records_h_and_the_decoders_agree_field_for_field() {
        let structs = btf_structs();
        let layouts = [
            layout!(
                RawArgCopy,
                "arg_copy",
                [kind, when, writes, count, count_arg, pad, size, limit]
            ),
            layout!(
                RawCallSpec,
                "call_spec",
                [selected, moves, makes, makes_arg, forks, pad, stat, args]
            ),
            layout!(
                RawFileRecord,
                "file_record",
                [header, fd, mode, ino, magic, rdev, len, cloexec, pad]
            ),
            layout!(RawConfig, "config", [flags, elements, pad]),
            layout!(
                RawEntry,
                "entry",
                [entered, args, nr, tgid, abi, flags, pad]
            ),
            layout!(RawRecord, "record", [kind, size, tid, tgid]),
            layout!(RawInFlightRecord, "in_flight_record", [tid, pad, entry]),
            layout!(
                RawCallRecord,
                "call_record",
                [header, entered, exited, ret, args, nr, abi, flags, captures]
            ),
            layout!(
                RawCaptureHeader,
                "capture_header",
                [arg, when, state, pad, len, wanted, pad2]
            ),
            layout!(
                RawSpawnRecord,
                "spawn_record",
                [header, time, child, child_tgid]
            ),
            layout!(RawExecRecord, "exec_record", [header, time, former, len]),
            layout!(RawExitRecord, "exit_record", [header, time, code, last]),
            layout!(
                RawSignalRecord,
                "signal_record",
                [header, time, signal, pad, info]
            ),
            layout!(
                RawStatValue,
                "stat_value",
                [
                    calls, errors, timed, total_ns, min_ns, max_ns, bytes, log2_ns
                ]
            ),
            layout!(RawErrnoKey, "errno_key", [slot, err]),
            layout!(
                RawStackRecord,
                "stack_record",
                [header, frames, copied, regs]
            ),
        ];
        for (name, size, fields) in layouts {
            let (c_size, c_fields) = structs
                .get(name)
                .unwrap_or_else(|| panic!("records.h has no struct {name}"));
            assert_eq!(*c_size as usize, size, "struct {name}'s size");
            let rust: Vec<(String, u32)> = fields
                .into_iter()
                .map(|(field, offset)| (field.to_owned(), u32::try_from(offset).expect("small")))
                .collect();
            assert_eq!(c_fields, &rust, "struct {name}'s fields");
        }
    }

    fn call_bytes(kind: u32, flags: u8, captures: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut tail = Vec::new();
        for &(arg, state, data) in captures {
            let header = RawCaptureHeader {
                arg,
                when: 1,
                state,
                len: u32::try_from(data.len()).expect("small"),
                wanted: if state == 1 { u32::MAX } else { 0 },
                ..RawCaptureHeader::default()
            };
            tail.extend_from_slice(header.as_bytes());
            tail.extend_from_slice(data);
            tail.resize(tail.len().next_multiple_of(8), 0);
        }
        let record = RawCallRecord {
            header: RawRecord {
                kind,
                size: u32::try_from(size_of::<RawCallRecord>() + tail.len()).expect("small"),
                tid: 11,
                tgid: 10,
            },
            entered: 100,
            exited: 250,
            ret: -2,
            args: [1, 2, 3, 4, 5, 6],
            nr: 257,
            abi: 0,
            flags,
            captures: u16::try_from(captures.len()).expect("small"),
        };
        let mut bytes = record.as_bytes().to_vec();
        bytes.extend_from_slice(&tail);
        bytes
    }

    #[test]
    fn call_records_decode_with_their_captures_and_reject_overruns() {
        let bytes = call_bytes(RECORD_CALL, 0, &[(1, 0, b"/etc/hosts"), (2, 1, b"abc")]);
        let Ok(Record::Call(call)) = decode(&bytes) else {
            panic!("a call");
        };
        assert_eq!(
            (call.thread, call.process, call.call.name(), call.outcome),
            (
                11,
                10,
                Some("openat"),
                Outcome::Failed(crate::syscalls::Errno(2))
            )
        );
        assert_eq!(call.duration(), Some(150));
        assert_eq!(&*call.captured[0].bytes, b"/etc/hosts");
        assert_eq!(
            call.captured[1].state,
            CaptureState::Truncated { wanted: u32::MAX }
        );

        // A thread that ended inside its call, whose entry was not seen.
        let bytes = call_bytes(RECORD_CALL, 2 | CALL_PROCESS_EXITED, &[]);
        let Ok(Record::Call(call)) = decode(&bytes) else {
            panic!("a call");
        };
        assert_eq!(
            (call.outcome, call.entered, call.exited),
            (Outcome::ProcessExited, None, None)
        );
        assert!(call.flags.contains(EventFlags::ENTRY_UNSEEN));

        let mut bytes = call_bytes(RECORD_CALL, 0, &[(1, 0, b"/etc/hosts")]);
        let header = size_of::<RawCallRecord>();
        bytes[header + 4] = 200;
        assert_eq!(decode(&bytes), Err(DecodeError::Capture(1)));
        assert!(decode(&bytes[..10]).is_err());
    }

    #[test]
    fn copy_rules_reach_the_kernel_as_the_ptrace_engine_follows_them() {
        let options = TraceOptions {
            calls: crate::syscalls::CallSet::none(),
            limits: captures::Limits::default(),
            verbose: false,
            follow: true,
            stacks: None,
        };
        let execve = SyscallId::named(Abi::X86_64, "execve").expect("execve");
        let spec = call_spec(execve, &options);
        // The path and argv at entry; the environment only counted.
        assert_eq!(
            spec.args[..3]
                .iter()
                .map(|arg| (arg.kind, arg.when))
                .collect::<Vec<_>>(),
            [(1, 0), (3, 0), (4, 0)]
        );
        let read = SyscallId::named(Abi::X86_64, "read").expect("read");
        let spec = call_spec(read, &options);
        assert_eq!(spec.moves, 1);
        assert_eq!(
            (spec.args[1].kind, spec.args[1].when, spec.args[1].count),
            (5, 1, 2)
        );
        assert!(spec.args.iter().all(|arg| arg.limit as usize <= MOST_BYTES));
    }

    #[test]
    fn statistics_fold_over_cpus_with_their_errnos() {
        let openat = SyscallId::named(Abi::X86_64, "openat").expect("openat");
        let mut one = RawStatValue {
            calls: 3,
            errors: 1,
            timed: 3,
            total_ns: 300,
            min_ns: 50,
            max_ns: 200,
            bytes: 0,
            log2_ns: [0; BUCKETS],
        };
        one.log2_ns[7] = 3;
        let mut two = one;
        two.min_ns = 20;
        let folded = fold(
            7,
            [(openat, vec![one, two])],
            [(
                RawErrnoKey {
                    // x86-64 calls fill the first slots.
                    slot: openat.number,
                    err: 2,
                },
                2,
            )],
        );
        let stats = &folded.by_call[&(7, openat)];
        assert_eq!(
            (stats.calls, stats.min_ns, stats.max_ns, stats.log2_ns[7]),
            (6, 20, 200, 6)
        );
        assert_eq!(stats.errors_by_errno[&2], 2);
    }

    #[test]
    fn the_probe_names_what_is_missing() {
        let probe = Probe {
            release: "6.8.0".into(),
            version: Some((6, 8)),
            btf: true,
            capabilities: 0,
            lockdown: None,
        };
        let message = probe.check().expect_err("unprivileged").to_string();
        assert!(message.contains("has neither"), "{message}");
        assert!(message.contains("--engine ptrace"), "{message}");
        let able = Probe {
            capabilities: (1 << CAP_BPF) | (1 << CAP_PERFMON),
            ..probe
        };
        assert!(able.check().is_ok());
        let old = Probe {
            version: version("5.10.0-23-amd64"),
            ..able
        };
        assert!(old.check().expect_err("old").to_string().contains("5.15"));
        assert_eq!(version("7.2.5-200.fc44.x86_64"), Some((7, 2)));
    }
}
