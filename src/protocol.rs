use std::{ffi::OsString, fmt, path::PathBuf, process::Stdio, sync::Arc};

use tokio::sync::oneshot;

use crate::model::numeric_id;

use crate::{
    AddressDescription, Backtrace, BreakpointLocation, CodeInstanceId, DereferenceReference,
    DereferencedValue, ExecutionLocation, GlobalVariablePage, GlobalVariableReference, LineNumber,
    LoadedModule, LoadedModuleSnapshot, RegisterSnapshot, Result, StackFrame, StackFrameId,
    ThreadId, ValueChildPage, ValueChildrenReference, VariableSnapshot, VirtualAddress,
};

/// Selects data objects to inspect in one frame of a stopped thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableQuery {
    /// Inspect every visible parameter and local declaration.
    All,
    /// Inspect the innermost visible data object with this name.
    Name(String),
    /// Inspect one exact global catalog entry.
    Global(GlobalVariableReference),
}

/// Selects one bounded page from the immutable global catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableQuery {
    /// Case-sensitive substring matched against source and linkage names.
    pub filter: Option<String>,
    /// Zero-based offset within the deterministic match ordering.
    pub offset: u64,
    /// Maximum entries to return; must be between 1 and 256.
    pub limit: u32,
}

impl Default for GlobalVariableQuery {
    fn default() -> Self {
        Self {
            filter: None,
            offset: 0,
            limit: 100,
        }
    }
}

/// Selects an arbitrary bounded page of one aggregate's children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildQuery {
    /// Zero-based offset within the parent's stable child ordering.
    pub offset: u64,
    /// Maximum children to return; must be between 1 and 256.
    pub limit: u32,
}

impl Default for ValueChildQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 32,
        }
    }
}

/// A view a value's type was matched against, in the order views are
/// tried, and why it did not bind, when it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCandidate {
    pub view: Arc<crate::ViewName>,
    pub rejection: Option<Arc<str>>,
}

/// The views whose patterns name one type: those tried until the first
/// that binds, and the `extend`s that add to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeViews {
    pub type_info: crate::TypeInfo,
    /// The module image that defines the type.
    pub module: Arc<std::path::Path>,
    pub candidates: Arc<[ViewCandidate]>,
}

impl TypeViews {
    /// The view that presents the type's values, when one binds.
    #[must_use]
    pub fn presented_by(&self) -> Option<&Arc<crate::ViewName>> {
        self.candidates
            .iter()
            .find(|candidate| candidate.rejection.is_none() && !candidate.view.extend)
            .map(|candidate| &candidate.view)
    }
}

/// How the loaded modules' types are presented: every type a view's pattern
/// names, and the views loaded for the session or carried by a module that
/// present none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCheck {
    pub types: Arc<[TypeViews]>,
    pub unused: Arc<[Arc<crate::ViewName>]>,
    /// The kernels loaded for the session or carried by a module, which
    /// their views may call.
    pub kernels: Arc<[KernelSource]>,
}

/// A kernel views may call, and what it is built from, so that it is
/// reviewed as source rather than trusted as a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSource {
    pub name: Arc<str>,
    /// The file or module record it was loaded from.
    pub origin: Arc<str>,
    /// Its source, or a link to it.
    pub source: Arc<str>,
}

/// Why a value is presented as it is: the views its type matched, and how
/// the one that binds presents the value at this stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewExplanation {
    /// The value's type.
    pub type_info: Option<crate::TypeInfo>,
    /// Whether views present values at all.
    pub enabled: bool,
    /// The views whose patterns name the type, until the first that binds.
    pub candidates: Arc<[ViewCandidate]>,
    /// The value's presentation at this stop, when a view binds.
    pub presentation: Option<Arc<crate::Presentation>>,
}

/// A user-facing request for a logical breakpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BreakpointSpec {
    /// Break at every concrete instance of the functions a location names;
    /// see [`crate::ModuleImage::functions_located`].
    Function(String),
    /// Break at every statement address of a source line. A line without
    /// statements moves to the next one with statements in the function
    /// containing it; see [`crate::ModuleImage::breakpoint_line`].
    Source { path: PathBuf, line: LineNumber },
    /// Break at every concrete instance of the functions a location names
    /// that are declared in one source file.
    FileFunction { path: PathBuf, function: String },
    /// Break at an absolute process virtual address.
    Address(VirtualAddress),
}

impl fmt::Display for BreakpointSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Function(function) => f.write_str(function),
            Self::Source { path, line } => write!(f, "{}:{line}", path.display()),
            Self::FileFunction { path, function } => write!(f, "{}:{function}", path.display()),
            Self::Address(address) => address.fmt(f),
        }
    }
}

numeric_id!(
    BreakpointId,
    "Identifies one logical user breakpoint within a debug session."
);

/// One deduplicated location resolved for a logical breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBreakpointLocation {
    /// The address and address space where the trap is installed.
    pub location: BreakpointLocation,
    /// Concrete code instances represented by this location.
    pub code_instances: Arc<[CodeInstanceId]>,
    /// The shared library the location is in, whose unloading removes it;
    /// `None` for the program itself and for explicit addresses.
    pub library: Option<crate::ModuleId>,
}

/// How a [`HitCondition`] compares a hit's number with its count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitComparison {
    /// The hit numbered `count`.
    Equal,
    /// Every hit except the one numbered `count`.
    NotEqual,
    /// Hits numbered below `count`.
    Less,
    /// Hits numbered up to and including `count`.
    LessOrEqual,
    /// Hits numbered above `count`.
    Greater,
    /// The hit numbered `count` and every later hit.
    GreaterOrEqual,
    /// Every hit whose number is a multiple of `count`.
    Multiple,
}

impl HitComparison {
    const fn operator(self) -> &'static str {
        match self {
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Multiple => "%",
        }
    }
}

/// Selects which hits of a breakpoint stop execution.
///
/// Each time a thread reaches any of a breakpoint's locations is one hit.
/// Hits are numbered from 1 per breakpoint, across all of its locations, and
/// hits that do not stop still count. A condition that no hit can meet is
/// rejected rather than kept as a breakpoint that silently never stops.
///
/// The text form is an operator followed by a decimal count, such as `>=5`,
/// `==3`, or `%10`. A bare count is rejected: debuggers disagree whether `N`
/// means only the Nth hit, the Nth and every later hit, or skipping N hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HitCondition {
    comparison: HitComparison,
    count: u64,
}

impl HitCondition {
    /// Creates a condition, rejecting one that no hit number can meet and a
    /// multiple of zero.
    pub fn new(comparison: HitComparison, count: u64) -> Result<Self> {
        let condition = Self { comparison, count };
        let satisfiable = match comparison {
            HitComparison::Equal | HitComparison::LessOrEqual | HitComparison::Multiple => {
                count != 0
            }
            HitComparison::Less => count > 1,
            HitComparison::Greater => count != u64::MAX,
            HitComparison::NotEqual | HitComparison::GreaterOrEqual => true,
        };
        if satisfiable {
            Ok(condition)
        } else {
            Err(crate::Error::InvalidHitCondition(format!(
                "no hit can satisfy {condition}"
            )))
        }
    }

    /// Returns how the condition compares hit numbers.
    #[must_use]
    pub const fn comparison(self) -> HitComparison {
        self.comparison
    }

    /// Returns the count hit numbers are compared with.
    #[must_use]
    pub const fn count(self) -> u64 {
        self.count
    }

    /// Whether the hit with this one-based number stops execution.
    #[must_use]
    pub const fn is_met(self, hit: u64) -> bool {
        match self.comparison {
            HitComparison::Equal => hit == self.count,
            HitComparison::NotEqual => hit != self.count,
            HitComparison::Less => hit < self.count,
            HitComparison::LessOrEqual => hit <= self.count,
            HitComparison::Greater => hit > self.count,
            HitComparison::GreaterOrEqual => hit >= self.count,
            HitComparison::Multiple => hit.is_multiple_of(self.count),
        }
    }

    /// Whether any hit after the first `hits` can still stop execution.
    #[must_use]
    pub const fn may_stop_after(self, hits: u64) -> bool {
        match self.comparison {
            HitComparison::Equal | HitComparison::LessOrEqual => hits < self.count,
            HitComparison::Less => hits.saturating_add(1) < self.count,
            HitComparison::Greater | HitComparison::GreaterOrEqual => hits < u64::MAX,
            HitComparison::NotEqual | HitComparison::Multiple => true,
        }
    }
}

impl fmt::Display for HitCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.comparison.operator(), self.count)
    }
}

impl std::str::FromStr for HitCondition {
    type Err = crate::Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || {
            let trimmed = text.trim();
            crate::Error::InvalidHitCondition(
                if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
                    format!(
                        "a bare count is ambiguous; write =={trimmed} to stop only at that hit \
                         or >={trimmed} to stop at it and every later hit"
                    )
                } else {
                    format!(
                        "'{text}' is not an operator (==, !=, <, <=, >, >=, %) followed by a \
                         decimal count"
                    )
                },
            )
        };
        // Two-character operators must be tried before their prefixes.
        let (comparison, digits) = [
            HitComparison::Equal,
            HitComparison::NotEqual,
            HitComparison::LessOrEqual,
            HitComparison::GreaterOrEqual,
            HitComparison::Less,
            HitComparison::Greater,
            HitComparison::Multiple,
        ]
        .into_iter()
        .find_map(|comparison| {
            text.trim()
                .strip_prefix(comparison.operator())
                .map(|digits| (comparison, digits.trim_start()))
        })
        .ok_or_else(invalid)?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        let count = digits.parse().map_err(|_| invalid())?;
        Self::new(comparison, count)
    }
}

/// An immutable logical breakpoint and all locations resolved for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: BreakpointId,
    /// The user intent that created the breakpoint.
    pub spec: BreakpointSpec,
    /// Every deduplicated location at which the breakpoint is installed.
    pub locations: Arc<[ResolvedBreakpointLocation]>,
    /// Which hits stop execution; `None` stops at every hit.
    pub hit_condition: Option<HitCondition>,
    /// A condition the hitting thread's innermost frame must meet for a
    /// hit that its hit condition allows to stop.
    pub condition: Option<crate::Condition>,
    /// A message logged, as [`DebuggerEvent::LogMessage`], at each hit
    /// that would stop, which then does not stop.
    pub log_message: Option<crate::LogMessage>,
    /// How many times threads of the current process reached the
    /// breakpoint, including hits its condition did not stop at. A new
    /// process starts again from zero.
    ///
    /// Hits that do not stop are resolved internally and publish nothing, so
    /// while the inferior runs the count advances without a new revision or
    /// [`DebuggerEvent::BreakpointsChanged`]. It is exact at every published
    /// stop and after the inferior exits.
    pub hit_count: u64,
}

/// What a breakpoint does at a hit besides counting it. Every hit is
/// counted; one stops when it meets the hit condition and the condition,
/// unless the breakpoint logs a message instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakpointOptions {
    /// Which hits may stop; `None` lets every hit stop.
    pub hit_condition: Option<HitCondition>,
    /// A condition the hitting thread's innermost frame must meet.
    pub condition: Option<crate::Condition>,
    /// A message to log instead of stopping.
    pub log_message: Option<crate::LogMessage>,
    /// Whether to keep a function or source breakpoint that no loaded module
    /// has code for yet, with no locations, until a module that has it
    /// loads. Without this, such a breakpoint is refused.
    pub pending: bool,
}

/// One part of a logged message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "a message has few parts, each made once per logged hit"
)]
pub enum LogPart {
    /// Literal text.
    Text(Arc<str>),
    /// A value the message shows, as the hitting thread saw it.
    Value {
        /// The value's expression.
        expression: crate::Expression,
        /// Its type, when it resolved.
        type_info: Option<crate::TypeInfo>,
        /// Its state; its capabilities belong to no stop.
        state: crate::VariableState,
    },
    /// A value the message names that could not be read.
    Error {
        /// The value's expression.
        expression: crate::Expression,
        /// Why it could not be read.
        error: Arc<str>,
    },
}

/// One logical breakpoint that a thread's hit stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakpointHit {
    /// The breakpoint whose hit condition the hit met.
    pub breakpoint: BreakpointId,
    /// The breakpoint's hit count including this hit, which is this hit's
    /// number. Other threads' hits counted before the stop was published
    /// can make [`Breakpoint::hit_count`] larger.
    pub hit_count: u64,
}

numeric_id!(
    WatchpointId,
    "Identifies one watchpoint within a debug session."
);

/// The breakpoint or watchpoint whose condition an event concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionOwner {
    Breakpoint(BreakpointId),
    Watchpoint(WatchpointId),
}

/// The memory accesses that trigger a watchpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WatchAccess {
    /// Stores that leave the watched bytes different from those the
    /// debugger last observed, as gdb's `watch` and lldb's `modify` report.
    ///
    /// The hardware traps every store, so a store of the bytes already
    /// there is resolved internally: the thread resumes without a stop or
    /// event. A change is judged again once every thread is stopped, and a
    /// store another thread undid before then is not reported either.
    Change,
    /// Stores to any watched byte, including stores of an identical value.
    Write,
    /// Loads from any watched byte, without stores.
    Read,
    /// Loads from or stores to any watched byte.
    ReadWrite,
}

impl fmt::Display for WatchAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Change => "change",
            Self::Write => "write",
            Self::Read => "read",
            Self::ReadWrite => "read/write",
        })
    }
}

/// What the debugger's hardware watchpoint support can arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointCapabilities {
    /// Hardware slots per thread, shared by every watchpoint. Other users of
    /// the debug hardware, such as perf breakpoints, can leave fewer.
    pub slots: u32,
    /// The widest naturally aligned span one slot covers. Other spans use
    /// several slots.
    pub max_slot_bytes: u64,
    /// The access kinds that can be watched, including changes judged from
    /// the stores the hardware reports.
    pub access: Arc<[WatchAccess]>,
}

/// The storage lifetime that bounds a watched object.
///
/// A scoped watchpoint is invalidated once its storage may belong to a
/// different object, instead of reporting accesses to unrelated data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    /// An explicit address, or storage reached through a pointer. The
    /// debugger cannot know when it is reused and never invalidates it.
    Location,
    /// Static storage owned by a loaded module.
    Static {
        /// The module whose unload invalidates the watchpoint.
        module: crate::ModuleId,
    },
    /// One thread's instance of thread-local storage.
    ThreadLocal {
        /// The thread whose exit invalidates the watchpoint.
        thread: ThreadId,
    },
    /// A local variable or parameter of one function activation.
    Frame {
        /// The thread executing the activation.
        thread: ThreadId,
        /// The activation's canonical frame address.
        activation: VirtualAddress,
    },
}

/// Debugger-internal evidence used to decide whether a frame-scoped object
/// is still live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameScopeEvidence {
    pub module: crate::ModuleId,
    pub image: crate::ModuleImageId,
    pub function: CodeInstanceId,
    pub ranges: Arc<[crate::AddressRange<crate::ImageAddress>]>,
}

/// An opaque capability for one watchable memory object resolved at one
/// stopped snapshot.
///
/// The address is fixed when the target is resolved, so watching an
/// expression that passes through a pointer keeps watching the original
/// pointee after the pointer changes. The target can only be armed at the
/// stop that resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub(crate) stop_id: StopId,
    pub(crate) expression: crate::Expression,
    pub(crate) address: VirtualAddress,
    pub(crate) byte_size: u64,
    pub(crate) type_info: Option<crate::TypeInfo>,
    pub(crate) scope: WatchScope,
    pub(crate) frame: Option<FrameScopeEvidence>,
}

impl WatchTarget {
    /// Returns the expression that named the object.
    #[must_use]
    pub const fn expression(&self) -> &crate::Expression {
        &self.expression
    }

    /// Returns the first watched byte.
    #[must_use]
    pub const fn address(&self) -> VirtualAddress {
        self.address
    }

    /// Returns the number of watched bytes.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Returns the lifetime that bounds the object's storage.
    #[must_use]
    pub const fn scope(&self) -> &WatchScope {
        &self.scope
    }
}

/// What a new watchpoint observes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchpointSpec {
    /// An object resolved at the current stop.
    Target(Box<WatchTarget>),
    /// Explicit bytes of the process address space.
    Location {
        /// The first watched byte.
        address: VirtualAddress,
        /// The number of watched bytes.
        byte_size: u64,
    },
}

/// An armed hardware watchpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watchpoint {
    pub id: WatchpointId,
    /// The accesses it reports.
    pub access: WatchAccess,
    /// The expression that named the object, absent for explicit locations.
    pub expression: Option<crate::Expression>,
    /// The first watched byte.
    pub address: VirtualAddress,
    /// The number of watched bytes.
    pub byte_size: u64,
    /// The watched object's type, when it was resolved from an expression.
    pub type_info: Option<crate::TypeInfo>,
    /// The lifetime that bounds the watched storage.
    pub scope: WatchScope,
    /// The naturally aligned hardware spans that exactly cover the bytes.
    pub coverage: Arc<[crate::AddressRange<VirtualAddress>]>,
    /// Which hits stop execution; `None` stops at every hit.
    pub hit_condition: Option<HitCondition>,
    /// A condition the accessing thread's innermost frame must meet, after
    /// the access, for a hit that its hit condition allows to stop.
    pub condition: Option<crate::Condition>,
    /// How many accesses the watchpoint reported, including those its
    /// conditions did not stop at: every access for [`WatchAccess::Write`]
    /// and [`WatchAccess::ReadWrite`], and every store that changed the
    /// bytes last observed for [`WatchAccess::Change`].
    ///
    /// As with [`Breakpoint::hit_count`], hits that do not stop publish
    /// nothing, so the count is exact at every published stop.
    pub hit_count: u64,
}

/// What a watchpoint does at a hit besides counting it: it stops when the
/// hit meets the hit condition and the condition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchpointOptions {
    /// Which hits may stop; `None` lets every hit stop.
    pub hit_condition: Option<HitCondition>,
    /// A condition the accessing thread's innermost frame must meet.
    pub condition: Option<crate::Condition>,
}

/// One watchpoint reported by one thread's access.
///
/// Hardware reports that an access happened, not what it changed: a store of
/// an identical value is reported with equal bytes, except by a
/// [`WatchAccess::Change`] watchpoint, whose hits always differ. Accesses
/// made by the kernel on the process's behalf, such as `read(2)` filling a
/// watched buffer, are never reported, so `previous` is the value last
/// observed by the debugger rather than necessarily the value immediately
/// before this access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointHit {
    /// The watchpoint that reported the access.
    pub watchpoint: WatchpointId,
    /// The thread whose instruction made the access.
    pub thread: ThreadId,
    /// The watchpoint's hit count including this hit, which is this hit's
    /// number. Other threads' hits counted before the stop was published
    /// can make [`Watchpoint::hit_count`] larger.
    pub hit_count: u64,
    /// The watched bytes last observed by the debugger, when readable.
    pub previous: Option<Arc<[u8]>>,
    /// The watched bytes once every thread stopped, when readable.
    pub current: Option<Arc<[u8]>>,
}

impl WatchpointHit {
    /// Whether the watched bytes differ from the last observed bytes.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.previous != self.current
    }
}

/// Why a scoped watchpoint stopped watching its storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchpointInvalidation {
    /// The function activation or lexical block owning the object ended.
    ScopeExited,
    /// The thread owning the object exited.
    OwnerThreadExited,
    /// The module owning the object was unloaded.
    ModuleUnloaded,
}

/// A watchpoint that was removed because its storage's lifetime ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidatedWatchpoint {
    /// The removed watchpoint.
    pub watchpoint: Watchpoint,
    /// Why its storage is no longer watched.
    pub reason: WatchpointInvalidation,
}

/// The logical frame selected for presentation at a stopped instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentedFrame {
    /// The physical containing frame is selected.
    Physical,
    /// One concrete inline instance is selected.
    Inline(CodeInstanceId),
    /// The debug metadata does not identify one compatible inline chain.
    Ambiguous(Arc<[CodeInstanceId]>),
}

/// Controller-owned logical presentation for the selected stopped thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePresentation {
    /// The machine instruction to which this presentation is tied.
    pub instruction: VirtualAddress,
    /// The logical frame currently selected at that instruction.
    pub frame: PresentedFrame,
    /// Active inline frames intentionally hidden below the selection.
    pub hidden_inline_frames: u32,
}

numeric_id!(
    ProcessId,
    "Identifies an inferior process; local attach accepts an operating-system process ID."
);

/// How a launched inferior's process is started.
///
/// The default runs the program with no arguments, in the debugger's working
/// directory and environment, sharing the debugger's standard streams.
#[derive(Debug, Default)]
pub struct LaunchOptions {
    /// Arguments passed after the program name.
    pub arguments: Vec<OsString>,
    /// Changes to the inherited environment, applied in order: a value sets
    /// the variable and `None` removes it.
    pub environment: Vec<(OsString, Option<OsString>)>,
    /// The inferior's working directory, instead of the debugger's.
    pub working_directory: Option<PathBuf>,
    /// The inferior's standard input, instead of the debugger's.
    pub stdin: Option<Stdio>,
    /// The inferior's standard output, instead of the debugger's.
    pub stdout: Option<Stdio>,
    /// The inferior's standard error, instead of the debugger's.
    pub stderr: Option<Stdio>,
    /// End the launch at the new process's first instruction, before the
    /// dynamic loader runs, with [`StopReason::Entry`].
    pub stop_at_entry: bool,
}

/// Selects a post-mortem core dump and how its module files are found and
/// trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreDumpOptions {
    /// The ELF core file to open.
    pub core: PathBuf,
    /// The executable that produced the dump. By default it is found like
    /// every other recorded module.
    pub executable: Option<PathBuf>,
    /// A directory holding the files of the machine that wrote the dump.
    ///
    /// Recorded paths are looked up inside it instead of on this machine.
    /// Every path, including absolute symbolic links and `..`, resolves as if
    /// the sysroot were `/`, so no file outside it is used.
    pub sysroot: Option<PathBuf>,
    /// Directories searched, in order, for module files missing from the
    /// recorded path or not matching the dump: first for a file with the
    /// recorded file name, then for any file with the recorded build-id.
    ///
    /// A file found here is used only when proven to match, unless module
    /// mismatches are allowed, so a directory may hold unrelated builds.
    pub module_paths: Vec<PathBuf>,
    /// Use module files that the dump cannot prove match its recorded images.
    ///
    /// A proven file is always preferred, then one the dump cannot verify,
    /// then one that provably differs. Such modules contribute debug metadata
    /// only: their file contents never substitute for memory the dump did not
    /// save. A file that cannot be placed at its recorded image is still
    /// refused, since relocating it would be a guess.
    pub allow_module_mismatch: bool,
}

impl CoreDumpOptions {
    /// Opens `core` with the files at its recorded paths on this machine and
    /// strict module identity.
    pub fn new(core: impl Into<PathBuf>) -> Self {
        Self {
            core: core.into(),
            executable: None,
            sysroot: None,
            module_paths: Vec::new(),
            allow_module_mismatch: false,
        }
    }
}

/// How a module file was matched to an image recorded in a core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleIdentity {
    /// The dumped GNU build-id note equals the file's.
    BuildId,
    /// Every saved byte of the file's read-only segments equals the file.
    SavedContent {
        /// The number of saved bytes compared.
        compared_bytes: u64,
    },
    /// The file differs from the dumped image; it was loaded only because
    /// module mismatches were explicitly allowed.
    Mismatched {
        /// Why the file is known to differ.
        detail: Arc<str>,
    },
    /// The dump saved nothing that could confirm the file; it was loaded only
    /// because module mismatches were explicitly allowed.
    Unverified,
}

impl ModuleIdentity {
    /// Whether the file is proven to be the dumped image.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::BuildId | Self::SavedContent { .. })
    }
}

/// The debugger's use of one image recorded in a core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreModuleState {
    /// A file was loaded for the image.
    Loaded {
        /// The loaded module and the file providing its metadata.
        module: crate::LoadedModuleRecord,
        /// The evidence that the file is the recorded image.
        identity: ModuleIdentity,
    },
    /// No file was found for the image; its frames and memory outside the
    /// dump stay unavailable.
    Missing,
}

/// One executable or shared-library image recorded in a core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreModule {
    /// The image path recorded by the dump.
    pub recorded_path: Arc<PathBuf>,
    /// The image's lowest mapped address at dump time.
    pub start: VirtualAddress,
    /// The GNU build-id the dump saved for the image, which identifies the
    /// file to supply when it is missing or mismatched.
    pub build_id: Option<Arc<[u8]>>,
    /// Whether and how a file was loaded for the image.
    pub state: CoreModuleState,
}

/// Immutable description of an opened post-mortem core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreDumpInfo {
    /// The core file.
    pub path: Arc<PathBuf>,
    /// The process that produced the dump.
    pub process_id: ProcessId,
    /// The process name recorded by the dump.
    pub process_name: Arc<str>,
    /// The leading command-line arguments recorded by the dump.
    pub arguments: Arc<str>,
    /// The signal that terminated the process, when recorded.
    pub exception: Option<ExceptionInfo>,
    /// Recorded images, beginning with the main executable.
    pub modules: Arc<[CoreModule]>,
}

numeric_id!(
    StopId,
    "Identifies an externally observable stopped snapshot."
);

numeric_id!(
    ExecutionId,
    "Identifies one accepted execution-control operation."
);

/// Selects which execution contexts a control operation resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeScope {
    /// Resume every eligible thread in one process.
    Process(ProcessId),
    /// Resume only one thread.
    Thread(ThreadId),
}

/// Selects the behavior of a stepping operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Execute one machine instruction.
    Instruction,
    /// Execute one machine instruction, running a call it makes until the
    /// call returns.
    OverInstruction,
    /// Advance to a different source location, entering calls.
    IntoSource,
    /// Advance to a different source location without stopping in callees.
    OverSource,
    /// Run until the selected frame returns to its caller.
    Out,
}

/// Selects what happens to an exception pending on a stopped thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionDisposition {
    /// Deliver a pending signal that its [`SignalPolicy`] passes.
    Pass,
    /// Discard the pending exception.
    Suppress,
}

/// What the debugger does when the inferior receives a signal.
///
/// Signals are named by their platform exception code; see
/// [`crate::signal_named`]. The defaults follow gdb: signals programs use
/// for routine work, such as `SIGALRM`, `SIGCHLD`, and Go's `SIGURG`,
/// neither stop nor print and are passed; `SIGINT` stops and is not
/// passed; every other signal stops and is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalPolicy {
    /// Stop every thread and report the signal as
    /// [`StopReason::Exception`].
    pub stop: bool,
    /// Report a signal that does not stop with
    /// [`DebuggerEvent::SignalReceived`].
    pub print: bool,
    /// Deliver the signal to the inferior; one not passed is discarded.
    pub pass: bool,
}

/// Platform-neutral information about an exception that stopped an inferior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionInfo {
    /// The platform-defined exception code.
    pub code: u64,
    /// A human-readable description of the exception.
    pub description: Arc<str>,
}

impl ExceptionInfo {
    /// Creates exception information from a platform code and description.
    pub fn new(code: u64, description: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }
}

/// Describes how an inferior process exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    /// The process returned an exit code.
    Code(i64),
    /// The process was terminated by an exception.
    Terminated(ExceptionInfo),
}

/// Describes why execution stopped or completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The debugger established an initial coherent stop after attaching.
    Attach,
    /// A launch requested with [`LaunchOptions::stop_at_entry`] stopped at
    /// the new process's first instruction.
    Entry,
    /// Execution reached an installed breakpoint.
    Breakpoint {
        address: VirtualAddress,
        /// Every logical breakpoint at the address whose hit condition this
        /// hit met, in identifier order.
        hits: Arc<[BreakpointHit]>,
    },
    /// A thread accessed watched memory. The thread stops after the
    /// accessing instruction, or within a repeated string instruction that
    /// has more iterations left.
    Watchpoint {
        /// Every watchpoint this thread's access reported.
        hits: Arc<[WatchpointHit]>,
    },
    /// Watched memory was accessed after the watched object's lifetime ended,
    /// so the affected watchpoints were removed instead of reporting
    /// accesses to unrelated data.
    WatchpointInvalidated {
        /// The removed watchpoints.
        invalidated: Arc<[InvalidatedWatchpoint]>,
    },
    /// A new thread could not be armed with the process's watchpoints. It
    /// was stopped before running so it never executes unwatched.
    WatchpointArmFailed {
        /// The unarmed thread.
        thread_id: ThreadId,
        /// Why arming failed.
        description: Arc<str>,
    },
    /// A stepping operation completed.
    Step { kind: StepKind },
    /// A stepping operation stopped before it could tell whether it had
    /// completed, because the debugger lost evidence it follows a step by,
    /// such as the stepping frame's caller. The thread stopped after the
    /// last instruction it executed, and may be resumed or stepped again.
    StepIncomplete {
        kind: StepKind,
        /// Why the step could not be followed.
        description: Arc<str>,
    },
    /// Execution stopped at the user's request.
    Pause,
    /// Execution stopped because of an exception.
    Exception(ExceptionInfo),
    /// The process replaced its executable image.
    Exec {
        /// Whether the new image is this debugger's executable, which is
        /// then loaded with every breakpoint in place. Any other image
        /// cannot be inspected or resumed.
        followed: bool,
    },
    /// A thread-specific execution operation ended because its thread exited.
    ThreadExited {
        /// The thread that exited.
        thread_id: ThreadId,
        /// How that thread exited. For a process's main thread, which exits
        /// before the process does, the code it passed to `exit`; the
        /// process's own status comes when it exits.
        status: ExitStatus,
    },
    /// The backend could not safely classify a native stop.
    Unclassifiable {
        /// Diagnostic details retained by the platform edge.
        description: Arc<str>,
    },
    /// The inferior exited.
    Exited(ExitStatus),
    /// A post-mortem core dump was opened; execution can never resume.
    CoreDump {
        /// The signal that terminated the process, when the dump recorded one.
        exception: Option<ExceptionInfo>,
    },
}

/// The externally observable execution state of one live thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadState {
    /// The thread is running or starting.
    Running,
    /// The thread is stopped and safe to inspect.
    Stopped {
        /// The thread's own stop reason, when it produced an interesting event.
        reason: Option<StopReason>,
    },
}

/// An immutable view of one thread at a debugger revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSnapshot {
    /// The thread being described.
    pub id: ThreadId,
    /// The thread's observable execution state.
    pub state: ThreadState,
    /// The name the thread gave itself, such as with
    /// `pthread_setname_np`, as of its start or the last stop. Core dumps
    /// record no thread names.
    pub name: Option<Arc<str>>,
}

/// The externally observable state of the inferior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferiorState {
    /// No inferior process exists.
    NotRunning,
    /// The inferior is starting or running.
    Running {
        /// The running process.
        process_id: ProcessId,
        /// The operation currently controlling execution, when known.
        execution_id: Option<ExecutionId>,
    },
    /// The inferior is stopped and may be inspected.
    Stopped {
        /// The stopped process.
        process_id: ProcessId,
        /// The immutable stopped snapshot identifier.
        stop_id: StopId,
        /// The thread selected by the stop.
        thread_id: ThreadId,
        /// The reason execution stopped.
        reason: StopReason,
    },
}

/// An immutable view of debugger state at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    /// The debugger revision represented by this snapshot.
    pub revision: u64,
    /// The current inferior state.
    pub inferior: InferiorState,
    /// The current stopped snapshot, when the inferior is stopped.
    pub stop_id: Option<StopId>,
    /// The thread selected for implicit inspection commands.
    pub selected_thread: Option<ThreadId>,
    /// The selected thread's frame that implicit inspection commands use:
    /// the innermost frame at each new stop, until a client selects another.
    pub selected_frame: Option<StackFrameId>,
    /// All live threads known at this revision.
    pub threads: Arc<[ThreadSnapshot]>,
    /// Logical presentation for the selected thread at this stop.
    pub presentation: Option<FramePresentation>,
    /// The logical breakpoints requested by clients.
    pub breakpoints: Arc<[Breakpoint]>,
    /// The watchpoints armed in the current process.
    pub watchpoints: Arc<[Watchpoint]>,
}

/// A state or lifecycle event emitted by the debugger.
///
/// Every event carries the state revision it produced; a
/// [`StateSnapshot`] at that revision or later reflects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerEvent {
    /// Debugger state changed in a way no more specific event describes.
    StateChanged { revision: u64 },
    /// A launched inferior began executing under `execution_id`.
    InferiorLaunched {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
    },
    /// An existing process was attached and coherently stopped.
    InferiorAttached {
        revision: u64,
        process_id: ProcessId,
    },
    /// Stopped threads resumed under `execution_id`.
    InferiorContinued {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
        resumed: ResumeScope,
    },
    /// Every live thread is stopped and safe to inspect.
    InferiorStopped {
        revision: u64,
        process_id: ProcessId,
        /// The execution that ended, or `None` for a stop no client caused.
        execution_id: Option<ExecutionId>,
        stop_id: StopId,
        /// The thread whose event caused the stop.
        thread_id: ThreadId,
        reason: StopReason,
    },
    /// A new thread appeared in the inferior.
    ThreadStarted {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
    },
    /// One thread of a still-running inferior exited. A main thread that
    /// exits before the process does is reported when it exits, with the
    /// code it passed to `exit`, though the platform reports its status only
    /// with the process's.
    ThreadExited {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        status: ExitStatus,
    },
    /// A shared object was mapped into the inferior.
    ModuleLoaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },
    /// A shared object was unmapped from the inferior.
    ModuleUnloaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },
    /// The inferior process exited.
    InferiorExited {
        revision: u64,
        process_id: ProcessId,
        /// The execution that ended, when one was active.
        execution_id: Option<ExecutionId>,
        status: ExitStatus,
    },
    /// The debugger released an attached process, which keeps running.
    InferiorDetached {
        revision: u64,
        process_id: ProcessId,
    },
    /// A thread received a signal whose policy neither stops nor discards
    /// its report: it was delivered or discarded and the thread ran on.
    SignalReceived {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        exception: ExceptionInfo,
    },
    /// A thread hit a breakpoint that logs instead of stopping.
    LogMessage {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        breakpoint: BreakpointId,
        parts: Arc<[LogPart]>,
    },
    /// A breakpoint's or watchpoint's condition could not be evaluated at a
    /// hit, which therefore stops as if the condition were met.
    ConditionFailed {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        owner: ConditionOwner,
        error: Arc<str>,
    },
    /// The set of logical breakpoints changed.
    BreakpointsChanged { revision: u64 },
    /// The set of armed watchpoints changed.
    WatchpointsChanged { revision: u64 },
    /// Scoped watchpoints were removed because their storage's lifetime
    /// ended. A following `WatchpointsChanged` publishes the new set.
    WatchpointsInvalidated {
        revision: u64,
        invalidated: Arc<[InvalidatedWatchpoint]>,
    },
}

impl DebuggerEvent {
    /// Returns the state revision this event produced.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        match self {
            Self::StateChanged { revision }
            | Self::InferiorLaunched { revision, .. }
            | Self::InferiorAttached { revision, .. }
            | Self::InferiorContinued { revision, .. }
            | Self::InferiorStopped { revision, .. }
            | Self::ThreadStarted { revision, .. }
            | Self::ThreadExited { revision, .. }
            | Self::ModuleLoaded { revision, .. }
            | Self::ModuleUnloaded { revision, .. }
            | Self::InferiorExited { revision, .. }
            | Self::InferiorDetached { revision, .. }
            | Self::SignalReceived { revision, .. }
            | Self::LogMessage { revision, .. }
            | Self::ConditionFailed { revision, .. }
            | Self::BreakpointsChanged { revision }
            | Self::WatchpointsChanged { revision }
            | Self::WatchpointsInvalidated { revision, .. } => *revision,
        }
    }
}

/// Carries one request's result back to the client awaiting it.
pub type Reply<T> = oneshot::Sender<Result<T>>;

/// A client request to the controller; each carries its reply channel.
pub enum Request {
    AddBreakpoint {
        spec: BreakpointSpec,
        options: Box<BreakpointOptions>,
        reply: Reply<Breakpoint>,
    },
    SetBreakpointHitCondition {
        id: BreakpointId,
        hit_condition: Option<HitCondition>,
        reply: Reply<Breakpoint>,
    },
    SetBreakpointCondition {
        id: BreakpointId,
        condition: Option<crate::Condition>,
        reply: Reply<Breakpoint>,
    },
    RemoveBreakpoint {
        id: BreakpointId,
        reply: Reply<Breakpoint>,
    },
    RemoveAllBreakpoints {
        reply: Reply<Arc<[Breakpoint]>>,
    },
    ResolveWatchTarget {
        expression: crate::Expression,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<WatchTarget>,
    },
    AddWatchpoint {
        spec: WatchpointSpec,
        access: WatchAccess,
        options: WatchpointOptions,
        reply: Reply<Watchpoint>,
    },
    SetWatchpointHitCondition {
        id: WatchpointId,
        hit_condition: Option<HitCondition>,
        reply: Reply<Watchpoint>,
    },
    SetWatchpointCondition {
        id: WatchpointId,
        condition: Option<crate::Condition>,
        reply: Reply<Watchpoint>,
    },
    RemoveWatchpoint {
        id: WatchpointId,
        reply: Reply<Watchpoint>,
    },
    RemoveAllWatchpoints {
        reply: Reply<Arc<[Watchpoint]>>,
    },
    Launch {
        options: Box<LaunchOptions>,
        reply: Reply<ExecutionId>,
    },
    Attach {
        process_id: ProcessId,
        reply: Reply<StopId>,
    },
    LaunchByExec {
        process_id: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    },
    Continue {
        process_id: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },
    Step {
        process_id: ProcessId,
        stop_id: StopId,
        thread_id: ThreadId,
        /// The frame whose return ends [`StepKind::Out`]; every other kind
        /// steps the innermost frame and requires it.
        frame: StackFrameId,
        kind: StepKind,
        /// The threads that run while the step does: every thread, or only
        /// the stepping one.
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },
    Pause {
        process_id: ProcessId,
        reply: Reply<ExecutionId>,
    },
    ReadMemory {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        byte_count: u64,
        reply: Reply<crate::MemoryRead>,
    },
    WriteMemory {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        bytes: Arc<[u8]>,
        reply: Reply<u64>,
    },
    LoadedModule {
        reply: Reply<LoadedModule>,
    },
    LoadedModules {
        reply: Reply<LoadedModuleSnapshot>,
    },
    ModuleImage {
        module: crate::ModuleId,
        reply: Reply<Arc<crate::ModuleImage>>,
    },
    Disassemble {
        query: crate::DisassemblyQuery,
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<crate::Disassembly>,
    },
    DescribeAddress {
        stop_id: StopId,
        address: VirtualAddress,
        reply: Reply<AddressDescription>,
    },
    StoppedLocation {
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<ExecutionLocation>,
    },
    Snapshot {
        reply: Reply<StateSnapshot>,
    },
    /// The stop, thread, and frame that implicit inspection uses, without
    /// copying the rest of a snapshot.
    StoppedSelection {
        reply: Reply<crate::StoppedSelection>,
    },
    Backtrace {
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<Backtrace>,
    },
    Registers {
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<RegisterSnapshot>,
    },
    Variables {
        query: VariableQuery,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<VariableSnapshot>,
    },
    Evaluate {
        expression: crate::Expression,
        mode: crate::EvaluationMode,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<crate::Evaluation>,
    },
    ExpressionType {
        expression: crate::Expression,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<crate::TypeInfo>,
    },
    Dereference {
        reference: DereferenceReference,
        limits: crate::InspectionLimits,
        reply: Reply<DereferencedValue>,
    },
    ValueChildren {
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
        limits: crate::InspectionLimits,
        reply: Reply<ValueChildPage>,
    },
    Globals {
        query: GlobalVariableQuery,
        reply: Reply<GlobalVariablePage>,
    },
    /// Presents values with another set of views from now on.
    SetViews {
        views: Arc<crate::view::ViewSet>,
        reply: Reply<()>,
    },
    /// Turns presenting values with views on or off.
    EnableViews {
        enabled: bool,
        reply: Reply<()>,
    },
    ExplainView {
        expression: crate::Expression,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<ViewExplanation>,
    },
    /// Presents an expression's value and its first page of children,
    /// recording every kernel run it takes, each as text that replays it.
    RecordKernels {
        expression: crate::Expression,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<Vec<String>>,
    },
    /// The views whose patterns name the types a name means, in every
    /// loaded module.
    ExplainType {
        name: String,
        reply: Reply<Vec<TypeViews>>,
    },
    /// How every loaded module's types are presented.
    CheckViews {
        reply: Reply<ViewCheck>,
    },
    SelectThread {
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<()>,
    },
    SelectFrame {
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        reply: Reply<StackFrame>,
    },
    SignalPolicy {
        signal: u64,
        reply: Reply<SignalPolicy>,
    },
    Kill {
        reply: Reply<()>,
    },
    Terminate {
        reply: Reply<()>,
    },
    SetSignalPolicy {
        signal: u64,
        policy: SignalPolicy,
        reply: Reply<SignalPolicy>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

#[cfg(any(debug_assertions, test, feature = "sim"))]
impl Request {
    /// Names the request and what it acts on, for the flight recorder and
    /// the simulator's trace.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Launch { options, .. } => format!("launch {:?}", options.arguments),
            Self::Attach { process_id, .. } => format!("attach {process_id}"),
            Self::LaunchByExec {
                process_id,
                stop_at_entry,
                ..
            } => format!("launch by exec {process_id}, stop at entry {stop_at_entry}"),
            Self::Continue {
                stop_id,
                scope,
                exception,
                ..
            } => format!("continue {stop_id:?} {scope:?} {exception:?}"),
            Self::Step {
                stop_id,
                thread_id,
                frame,
                kind,
                scope,
                exception,
                ..
            } => {
                format!("step {kind:?} {stop_id:?} {thread_id:?} {frame:?} {scope:?} {exception:?}")
            }
            Self::Pause { process_id, .. } => format!("pause {process_id}"),
            Self::AddBreakpoint { spec, .. } => format!("add breakpoint {spec:?}"),
            Self::RemoveBreakpoint { id, .. } => format!("remove breakpoint {id:?}"),
            Self::SetBreakpointCondition { id, .. } => format!("set condition of {id:?}"),
            Self::SetBreakpointHitCondition { id, .. } => format!("set hit condition of {id:?}"),
            Self::AddWatchpoint { spec, access, .. } => {
                format!("add watchpoint {spec:?} {access:?}")
            }
            Self::SetWatchpointCondition { id, .. } => format!("set condition of {id:?}"),
            Self::SetWatchpointHitCondition { id, .. } => format!("set hit condition of {id:?}"),
            Self::RemoveWatchpoint { id, .. } => format!("remove watchpoint {id:?}"),
            Self::WriteMemory {
                stop_id,
                address,
                bytes,
                ..
            } => format!("write {} bytes at {address} {stop_id:?}", bytes.len()),
            Self::SetSignalPolicy { signal, policy, .. } => {
                format!("set signal policy {signal} {policy:?}")
            }
            Self::RemoveAllBreakpoints { .. } => "remove all breakpoints".to_owned(),
            Self::ResolveWatchTarget { .. } => "resolve watch target".to_owned(),
            Self::RemoveAllWatchpoints { .. } => "remove all watchpoints".to_owned(),
            Self::ReadMemory { .. } => "read memory".to_owned(),
            Self::LoadedModule { .. } => "loaded module".to_owned(),
            Self::LoadedModules { .. } => "loaded modules".to_owned(),
            Self::ModuleImage { .. } => "module image".to_owned(),
            Self::Disassemble { .. } => "disassemble".to_owned(),
            Self::DescribeAddress { .. } => "describe address".to_owned(),
            Self::StoppedLocation { .. } => "stopped location".to_owned(),
            Self::Snapshot { .. } => "snapshot".to_owned(),
            Self::StoppedSelection { .. } => "stopped selection".to_owned(),
            Self::Backtrace { .. } => "backtrace".to_owned(),
            Self::Registers { .. } => "registers".to_owned(),
            Self::Variables { .. } => "variables".to_owned(),
            Self::Evaluate { expression, .. } => format!("evaluate `{}`", expression.text()),
            Self::ExpressionType { expression, .. } => format!("type of `{}`", expression.text()),
            Self::Dereference { .. } => "dereference".to_owned(),
            Self::ValueChildren { .. } => "value children".to_owned(),
            Self::Globals { .. } => "globals".to_owned(),
            Self::SetViews { .. } => "set views".to_owned(),
            Self::EnableViews { enabled, .. } => format!("enable views {enabled}"),
            Self::ExplainView { expression, .. } => {
                format!("explain the view of `{}`", expression.text())
            }
            Self::RecordKernels { expression, .. } => {
                format!("record the kernels of `{}`", expression.text())
            }
            Self::ExplainType { name, .. } => format!("explain the views of `{name}`"),
            Self::CheckViews { .. } => "check views".to_owned(),
            Self::SelectThread { .. } => "select thread".to_owned(),
            Self::SelectFrame { .. } => "select frame".to_owned(),
            Self::SignalPolicy { .. } => "signal policy".to_owned(),
            Self::Kill { .. } => "kill".to_owned(),
            Self::Terminate { .. } => "terminate".to_owned(),
            Self::Shutdown { .. } => "shutdown".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPARISONS: [HitComparison; 7] = [
        HitComparison::Equal,
        HitComparison::NotEqual,
        HitComparison::Less,
        HitComparison::LessOrEqual,
        HitComparison::Greater,
        HitComparison::GreaterOrEqual,
        HitComparison::Multiple,
    ];

    #[test]
    fn hit_conditions_parse_their_display_form_and_select_hits() {
        let selected = |text: &str| {
            let condition = text.parse::<HitCondition>().expect(text);
            assert_eq!(
                condition.to_string().parse::<HitCondition>().expect(text),
                condition
            );
            (1..=12)
                .filter(|&hit| condition.is_met(hit))
                .collect::<Vec<_>>()
        };
        assert_eq!(selected("==3"), [3]);
        assert_eq!(selected(" != 3 "), [1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(selected("<3"), [1, 2]);
        assert_eq!(selected("<=3"), [1, 2, 3]);
        assert_eq!(selected(">10"), [11, 12]);
        assert_eq!(selected(">= 10"), [10, 11, 12]);
        assert_eq!(selected("%5"), [5, 10]);
        assert_eq!(selected(">=0"), (1..=12).collect::<Vec<_>>());
    }

    #[test]
    fn hit_conditions_reject_ambiguous_malformed_and_unsatisfiable_text() {
        let message = |text: &str| match text.parse::<HitCondition>() {
            Err(crate::Error::InvalidHitCondition(message)) => message,
            other => panic!("{text:?} parsed as {other:?}"),
        };
        assert!(message("5").contains("write ==5"), "{}", message("5"));
        for text in [
            "", "==", "= 5", "=>5", "<<5", "==-1", "==+1", "==5x", "==1e3", "%%2",
        ] {
            assert!(message(text).contains("not an operator"), "{text:?}");
        }
        assert!(message("==18446744073709551616").contains("not an operator"));
        for text in ["==0", "<1", "<0", "<=0", "%0", ">18446744073709551615"] {
            assert!(message(text).contains("no hit can satisfy"), "{text:?}");
        }
    }

    #[test]
    fn construction_accepts_exactly_the_conditions_some_hit_meets() {
        for comparison in COMPARISONS {
            for count in (0..8).chain([u64::MAX - 1, u64::MAX]) {
                let unchecked = HitCondition { comparison, count };
                let hits = (1..64).chain([u64::MAX - 1, u64::MAX]);
                assert_eq!(
                    HitCondition::new(comparison, count).is_ok(),
                    hits.clone().any(|hit| unchecked.is_met(hit)),
                    "{unchecked}"
                );
            }
        }
    }

    #[test]
    fn later_stops_are_predicted_exactly_by_the_conditions_hits() {
        for comparison in COMPARISONS {
            for count in 0..8 {
                let Ok(condition) = HitCondition::new(comparison, count) else {
                    continue;
                };
                for hits in 0..16 {
                    assert_eq!(
                        condition.may_stop_after(hits),
                        (hits + 1..64).any(|hit| condition.is_met(hit)),
                        "{condition} after {hits} hits"
                    );
                }
            }
        }
    }
}
