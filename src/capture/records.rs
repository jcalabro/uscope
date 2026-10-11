//! The records of a capture's tables.
//!
//! Every record is little-endian with
//! alignment one, so the rows in memory are the rows a capture file holds,
//! and records refer to each other by index, with [`NONE`] for none.

use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::CodeRole;

pub use crate::image::NONE;

/// What a table holds. The numbers are part of the format: a reader that
/// meets one it does not know rejects the chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum CaptureTable {
    /// NUL-terminated UTF-8 strings, named by offset.
    Strings = 1,
    /// What produced the capture, as key and value.
    Settings = 2,
    Modules = 3,
    Processes = 4,
    Threads = 5,
    Mappings = 6,
    Frames = 7,
    Stacks = 8,
    /// On-CPU samples, thread by thread, each thread's in time order.
    Samples = 9,
    /// Where each thread's on-CPU samples are.
    SampleRanges = 10,
    /// Each frame's source lines, innermost inline first: empty for a
    /// frame not symbolized.
    FrameSymbols = 11,
    Lines = 12,
    Functions = 13,
    /// Counts of what was lost, throttled, or truncated.
    Accounting = 14,
    /// Intervals a thread spent off its CPU, thread by thread.
    OffCpu = 15,
    /// Where each thread's off-CPU intervals are.
    OffCpuRanges = 16,
    /// Instrumentation sites.
    Sites = 17,
    /// Instrumented zones, thread by thread, depth by depth, by start.
    Zones = 18,
    /// Where each thread's zones are at each depth.
    ZoneRanges = 19,
    /// Values of plotted series, by series and time.
    Plots = 20,
    /// Instants: frames, messages, triggers, stops.
    Markers = 21,
    /// Runtime tasks: goroutines and async tasks.
    Tasks = 22,
    /// Samples of where tasks waited, thread of none, by time.
    TaskWaits = 23,
}

impl CaptureTable {
    pub const ALL: [Self; 23] = [
        Self::Strings,
        Self::Settings,
        Self::Modules,
        Self::Processes,
        Self::Threads,
        Self::Mappings,
        Self::Frames,
        Self::Stacks,
        Self::Samples,
        Self::SampleRanges,
        Self::FrameSymbols,
        Self::Lines,
        Self::Functions,
        Self::Accounting,
        Self::OffCpu,
        Self::OffCpuRanges,
        Self::Sites,
        Self::Zones,
        Self::ZoneRanges,
        Self::Plots,
        Self::Markers,
        Self::Tasks,
        Self::TaskWaits,
    ];

    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        self as usize - 1
    }

    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u32 == number)
    }
}

/// A key and its value, both strings.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Setting {
    pub key: U32,
    pub value: U32,
}

/// A file the capture's code came from.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Module {
    /// Its path when it was mapped.
    pub path: U32,
    /// Its GNU build id in hex, or [`NONE`] for a file without one.
    pub build_id: U32,
    /// XXH3-128 of its bytes, or zero when they could not be read.
    pub digest: [u8; 16],
    pub size: U64,
}

/// A process the capture saw.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Process {
    pub pid: U32,
    pub name: U32,
    pub start: I64,
    pub end: I64,
}

/// A thread the capture saw.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Thread {
    pub process: U32,
    pub tid: U32,
    pub name: U32,
    pub reserved: U32,
    pub start: I64,
    pub end: I64,
}

/// One mapping of a module into a process.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Mapping {
    pub process: U32,
    pub module: U32,
    pub start: U64,
    pub end: U64,
    pub file_offset: U64,
    /// What the process adds to the module's image addresses.
    pub bias: U64,
    pub time: I64,
}

/// What a frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FrameKind {
    /// An instruction of a module, by image address; or, with no module, a
    /// virtual address no module covers.
    Native = 0,
    /// Where a stack's unwinding ended without reaching its outermost
    /// frame, and why: the address is a [`Truncation`]'s number, and the
    /// module, when there is one, the module at fault.
    Truncated = 1,
    /// The task a task segment of a stack belongs to, by its root frame.
    TaskRoot = 2,
    /// The runtime's scheduler, standing for every frame below a task.
    Runtime = 3,
}

impl FrameKind {
    pub const fn from_number(number: u8) -> Option<Self> {
        match number {
            0 => Some(Self::Native),
            1 => Some(Self::Truncated),
            2 => Some(Self::TaskRoot),
            3 => Some(Self::Runtime),
            _ => None,
        }
    }
}

/// Why a stack ends without reaching its outermost frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Truncation {
    /// No module's call-frame information covers the code.
    NoUnwindInfo = 1,
    /// The kernel's frame-pointer chain passes through a function that
    /// does not keep its frame pointer, so its callers are unknown.
    FramePointerNotKept = 2,
    /// The copy of the stack ended before a frame the kernel's chain could
    /// continue from.
    StackCopyExhausted = 3,
    /// A return address no mapping covers.
    UnmappedAddress = 4,
    /// The deepest stack recorded.
    DepthLimit = 5,
    /// The kernel's chain ended at a frame that is not a stack's outermost.
    ChainEnded = 6,
    /// Unwinding found an impossible caller.
    InvalidCaller = 7,
    /// The sample carried no stack at all.
    NoStack = 8,
    /// A runtime switched stacks, and neither the kernel's chain nor the
    /// copy reaches the frames it switched from.
    StackSwitch = 9,
}

impl Truncation {
    pub const ALL: [Self; 9] = [
        Self::NoUnwindInfo,
        Self::FramePointerNotKept,
        Self::StackCopyExhausted,
        Self::UnmappedAddress,
        Self::DepthLimit,
        Self::ChainEnded,
        Self::InvalidCaller,
        Self::NoStack,
        Self::StackSwitch,
    ];

    pub fn from_number(number: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u64 == number)
    }

    /// How a view names the truncation.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NoUnwindInfo => "no unwind information",
            Self::FramePointerNotKept => "frame pointer not kept",
            Self::StackCopyExhausted => "stack copy exhausted",
            Self::UnmappedAddress => "unmapped return address",
            Self::DepthLimit => "depth limit",
            Self::ChainEnded => "frame chain ended",
            Self::InvalidCaller => "invalid caller",
            Self::NoStack => "no stack",
            Self::StackSwitch => "frames before a stack switch not captured",
        }
    }
}

/// One distinct frame.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Frame {
    pub address: U64,
    pub module: U32,
    /// A [`FrameKind`].
    pub kind: u8,
    /// The function's `CodeRole`, by its number, when known.
    pub role: u8,
    pub reserved: U16,
}

/// A node of the stack tree: a frame called from its parent, or a root.
/// Parents come before their children.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Stack {
    pub parent: U32,
    pub frame: U32,
}

/// One sample: when, how much, and the stack it caught.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Sample {
    /// Nanoseconds on the capture's clock.
    pub time: I64,
    /// Nanoseconds of CPU time, or of time off the CPU, the sample stands
    /// for.
    pub weight: U64,
    pub stack: U32,
    /// The task it ran, or [`NONE`].
    pub task: U32,
}

/// Where one thread's rows are in a table grouped by thread.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ThreadRange {
    pub thread: U32,
    pub start: U32,
    pub count: U32,
}

/// Where a frame's lines are: `count` rows from `start`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Range {
    pub start: U32,
    pub count: U32,
}

/// A source line, of a function and possibly inlined into the next row's.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Line {
    pub function: U32,
    /// The file's path, or [`NONE`].
    pub file: U32,
    pub number: U32,
    pub column: U32,
}

/// A function frames run.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Function {
    pub name: U32,
    pub module: U32,
    /// The file it is declared in, or [`NONE`].
    pub file: U32,
    pub line: U32,
    /// Its `CodeRole`, by [`role_number`].
    pub role: u8,
    pub reserved: [u8; 3],
}

/// A role's number in a capture.
pub const fn role_number(role: CodeRole) -> u8 {
    match role {
        CodeRole::Ordinary => 0,
        CodeRole::Wrapper => 1,
        CodeRole::RuntimeInternal => 2,
        CodeRole::Panic => 3,
        CodeRole::StackSwitch => 4,
        CodeRole::Outermost => 5,
        CodeRole::TrapEntry => 6,
        CodeRole::SignalTrampoline => 7,
        CodeRole::Dispatch => 8,
    }
}

/// The role a capture's number stands for.
pub const fn role_of(number: u8) -> Option<CodeRole> {
    Some(match number {
        0 => CodeRole::Ordinary,
        1 => CodeRole::Wrapper,
        2 => CodeRole::RuntimeInternal,
        3 => CodeRole::Panic,
        4 => CodeRole::StackSwitch,
        5 => CodeRole::Outermost,
        6 => CodeRole::TrapEntry,
        7 => CodeRole::SignalTrampoline,
        8 => CodeRole::Dispatch,
        _ => return None,
    })
}

/// What a count in [`Account`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum AccountKind {
    /// Samples the kernel dropped because a ring was full.
    LostSamples = 1,
    /// Times the kernel throttled a sampling event.
    Throttles = 2,
    /// Samples whose stack ended in a truncation, by its number above
    /// 1000: 1001 is [`Truncation::NoUnwindInfo`].
    Truncated = 1000,
    /// Records uscope could not decode.
    Malformed = 3,
    /// Bytes of stack the kernel copied.
    StackBytes = 7,
    /// Records the kernel wrote after later ones had been processed, which
    /// were applied late: a mapping's samples before it end unmapped.
    LateRecords = 11,
    /// Breakpoint samples of an instruction that ran again within one
    /// call, as a Go stack check does when it grows the stack.
    Repeats = 12,
}

/// A count of something the views must show.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Account {
    pub kind: U32,
    pub reserved: U32,
    pub value: U64,
}

/// One instrumentation site.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Site {
    pub name: U32,
    pub file: U32,
    pub line: U32,
    pub module: U32,
    /// The site record's kind byte.
    pub kind: u8,
    pub reserved: [u8; 3],
}

/// One instrumented zone.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Zone {
    pub start: I64,
    pub end: I64,
    pub site: U32,
    /// Zone flags.
    pub flags: U32,
}

/// Where one thread's zones of one depth are.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ZoneRange {
    pub thread: U32,
    pub depth: U32,
    pub start: U32,
    pub count: U32,
}

/// One value of a plotted series.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Plot {
    pub time: I64,
    /// The value's bits, as an `f64`.
    pub value: U64,
    pub site: U32,
    pub thread: U32,
}

/// An instant on a thread's timeline.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Marker {
    pub time: I64,
    pub thread: U32,
    pub site: U32,
    /// A string, or [`NONE`].
    pub text: U32,
    pub kind: u8,
    pub reserved: [u8; 3],
}

/// A task of a language runtime.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Task {
    /// The runtime's name, as a string: "goroutine", "task".
    pub runtime: U32,
    pub number: U64,
    /// The task's root frame, or [`NONE`].
    pub root: U32,
    /// Its labels, as a string `key=value` per line, or [`NONE`].
    pub labels: U32,
    pub first_seen: I64,
    pub last_seen: I64,
}

/// One task snapshot entry: a task waiting at a stack.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct TaskWait {
    pub time: I64,
    /// The time it stands for.
    pub weight: U64,
    pub task: U32,
    pub stack: U32,
    /// The runtime's state, as a string.
    pub state: U32,
    pub reserved: U32,
}

macro_rules! records {
    ($($record:ty => $table:ident),* $(,)?) => {
        /// Each table's record size, for the directory.
        pub const fn stride(table: CaptureTable) -> usize {
            match table {
                CaptureTable::Strings => 1,
                CaptureTable::SampleRanges => size_of::<ThreadRange>(),
                CaptureTable::OffCpu => size_of::<Sample>(),
                CaptureTable::OffCpuRanges => size_of::<ThreadRange>(),
                $(CaptureTable::$table => size_of::<$record>(),)*
            }
        }

        /// The schema as text, which fingerprints every record's layout.
        pub fn schema_text() -> String {
            let mut text = String::from("capture\n");
            $(text.push_str(&format!(
                "{} {} {}\n",
                CaptureTable::$table as u32,
                stringify!($record),
                size_of::<$record>()
            ));)*
            text
        }
    };
}

records! {
    Setting => Settings,
    Module => Modules,
    Process => Processes,
    Thread => Threads,
    Mapping => Mappings,
    Frame => Frames,
    Stack => Stacks,
    Sample => Samples,
    Range => FrameSymbols,
    Line => Lines,
    Function => Functions,
    Account => Accounting,
    Site => Sites,
    Zone => Zones,
    ZoneRange => ZoneRanges,
    Plot => Plots,
    Marker => Markers,
    Task => Tasks,
    TaskWait => TaskWaits,
}

const _: () = assert!(size_of::<Frame>() == 16);
const _: () = assert!(size_of::<Stack>() == 8);
const _: () = assert!(size_of::<Sample>() == 24);
const _: () = assert!(size_of::<Zone>() == 24);
