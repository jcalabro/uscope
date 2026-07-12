use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Error, Result};

macro_rules! address_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Creates an address from its numeric representation.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the numeric representation of this address.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#x}", self.0)
            }
        }

        impl fmt::LowerHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }

        impl fmt::UpperHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::UpperHex::fmt(&self.0, f)
            }
        }
    };
}

address_type!(
    ImageAddress,
    "An address in the address space described by a module image."
);
address_type!(
    VirtualAddress,
    "An address in the virtual address space of a running process."
);

/// A half-open address range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange<A> {
    /// The first address included in the range.
    pub start: A,
    /// The first address after the range.
    pub end: A,
}

impl<A: Copy + Ord> AddressRange<A> {
    /// Returns whether the address is contained in this range.
    #[must_use]
    pub fn contains(self, address: A) -> bool {
        self.start <= address && address < self.end
    }
}

macro_rules! id_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            pub(crate) const fn new(value: u32) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    ModuleImageId,
    "Identifies a module image within a debug session."
);
id_type!(
    ModuleId,
    "Identifies a loaded module within a debug session."
);
id_type!(FunctionId, "Identifies a function within a module image.");
id_type!(
    CodeInstanceId,
    "Identifies one concrete code instance within a module image."
);
id_type!(
    LineSequenceId,
    "Identifies one contiguous line-program sequence within a module image."
);
id_type!(
    SourceFileId,
    "Identifies a source file within a module image."
);
id_type!(
    SymbolId,
    "Identifies a linker symbol within a module image."
);
id_type!(
    StackFrameId,
    "Identifies a stack frame within one stop revision."
);
id_type!(
    RegisterId,
    "Identifies a register within a target architecture."
);

/// Identifies a thread within a debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(u64);

impl ThreadId {
    /// Creates a thread identifier from its platform value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the platform value of this identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The architecture-independent purpose of a distinguished register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterRole {
    /// The address of the current instruction.
    ProgramCounter,
    /// The address of the top of the current stack.
    StackPointer,
    /// The base address conventionally used for the current stack frame.
    FramePointer,
}

/// Static information about a target register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterDescriptor {
    /// The register's identifier within its target architecture.
    pub id: RegisterId,
    /// The canonical architecture-defined register name.
    pub name: Arc<str>,
    /// The number of meaningful bits in the register.
    pub bits: u16,
    /// The register's architecture-independent role, when distinguished.
    pub role: Option<RegisterRole>,
}

/// One target register and its captured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterValue {
    /// The register represented by this value.
    pub register: RegisterDescriptor,
    /// The register bytes in the target's byte order.
    pub bytes: Arc<[u8]>,
}

/// The general register set of a stopped thread at one debugger revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// The debugger revision at which these values were read.
    pub revision: u64,
    /// The thread whose registers were read.
    pub thread: ThreadId,
    /// The architecture and data representation of the register values.
    pub target: TargetDescription,
    /// Register values in the architecture's canonical display order.
    pub registers: Arc<[RegisterValue]>,
}

/// The source-language encoding of a scalar base type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BaseTypeEncoding {
    /// A truth value.
    Boolean,
    /// A signed integer.
    Signed,
    /// A signed character integer.
    SignedCharacter,
    /// An unsigned integer.
    Unsigned,
    /// An unsigned character integer.
    UnsignedCharacter,
    /// A binary floating-point value.
    Floating,
}

/// A resolved scalar type independent of its debug-information encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseType {
    /// The source-facing type name.
    pub name: Arc<str>,
    /// The underlying base-type name, before typedef presentation.
    pub base_name: Arc<str>,
    /// How values of the type are encoded.
    pub encoding: BaseTypeEncoding,
    /// The number of bytes occupied in target storage.
    pub byte_size: u64,
}

/// Exact target bits for a supported floating-point value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FloatValue {
    /// IEEE binary32 bits.
    Binary32(u32),
    /// IEEE binary64 bits.
    Binary64(u64),
    /// The meaningful 80 bits of an x87 extended value.
    X87Extended {
        /// The explicit integer bit and fraction.
        significand: u64,
        /// The sign and biased exponent.
        sign_exponent: u16,
    },
}

/// A decoded scalar value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScalarValue {
    /// A C truth value.
    Boolean(bool),
    /// A sign-extended integer.
    Signed(i128),
    /// An unsigned integer.
    Unsigned(u128),
    /// A binary floating-point value retained as exact target bits.
    Floating(FloatValue),
}

/// The storage containing a variable's current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableStorage {
    /// Memory in the inferior's virtual address space.
    Memory(VirtualAddress),
}

/// Why valid variable metadata cannot produce a value at this stop.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableUnavailableReason {
    /// The call-frame information uses a CFA expression not yet supported.
    CfaExpression,
    /// Another explicit limitation or runtime failure.
    Other(Arc<str>),
}

impl fmt::Display for VariableUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CfaExpression => formatter.write_str("CFA expressions are unsupported"),
            Self::Other(description) => formatter.write_str(description),
        }
    }
}

impl From<Arc<str>> for VariableUnavailableReason {
    fn from(description: Arc<str>) -> Self {
        Self::Other(description)
    }
}

impl From<&str> for VariableUnavailableReason {
    fn from(description: &str) -> Self {
        Self::Other(description.into())
    }
}

impl From<String> for VariableUnavailableReason {
    fn from(description: String) -> Self {
        Self::Other(description.into())
    }
}

/// Why one variable's debug metadata is defective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableMalformedReason {
    /// A stable human-readable diagnosis.
    pub description: Arc<str>,
}

/// The inspection state of one visible variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableState {
    /// The value was read and decoded exactly.
    Available {
        /// Where the bytes were read.
        storage: VariableStorage,
        /// Exact bytes in target byte order, including ABI padding.
        raw: Arc<[u8]>,
        /// The decoded scalar value.
        value: ScalarValue,
    },
    /// Valid metadata does not provide a supported readable value here.
    Unavailable(VariableUnavailableReason),
    /// This entry's metadata is defective.
    Malformed(VariableMalformedReason),
}

/// One local variable visible in the selected stopped frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// The source-level variable name.
    pub name: Arc<str>,
    /// Its declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,
    /// Its resolved scalar type, when valid and supported.
    pub type_info: Option<BaseType>,
    /// Its current availability and value.
    pub state: VariableState,
}

/// Variables inspected from one stopped thread snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableSnapshot {
    /// The debugger revision at which the values were read.
    pub revision: u64,
    /// The stopped snapshot that authorized the reads.
    pub stop_id: crate::StopId,
    /// The thread whose top physical frame was inspected.
    pub thread: ThreadId,
    /// Target data representation used for decoding.
    pub target: TargetDescription,
    /// Visible variables in source declaration order.
    pub variables: Arc<[Variable]>,
}

/// The target CPU architecture described by a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Architecture {
    /// The x86-64 architecture.
    X86_64,
    /// The 64-bit Arm architecture.
    Aarch64,
}

/// The byte order used by the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Least-significant byte first.
    Little,
    /// Most-significant byte first.
    Big,
}

/// The width of an address on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerWidth {
    /// A 32-bit address.
    Bits32,
    /// A 64-bit address.
    Bits64,
}

/// Platform-independent properties of a debug target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetDescription {
    /// The target CPU architecture.
    pub architecture: Architecture,
    /// The target byte order.
    pub byte_order: ByteOrder,
    /// The width of target addresses.
    pub pointer_width: PointerWidth,
}

/// A one-based source line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineNumber(u64);

impl LineNumber {
    /// Creates a line number, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the numeric line number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LineNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A one-based source column number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnNumber(u64);

impl ColumnNumber {
    /// Creates a column number, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the numeric column number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ColumnNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A source file referenced by debug information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// The file's session-scoped identifier.
    pub id: SourceFileId,
    /// The source path resolved from the debug metadata.
    pub path: Arc<PathBuf>,
}

/// A location in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// The source file containing the location.
    pub file: SourceFileId,
    /// The one-based source line.
    pub line: LineNumber,
    /// The one-based column, when present in the debug information.
    pub column: Option<ColumnNumber>,
}

/// One numbered line read from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    /// The one-based line number.
    pub number: LineNumber,
    /// The source text without its line terminator.
    pub text: Arc<str>,
}

/// Source lines surrounding an execution location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContext {
    /// The source file that was read.
    pub file: SourceFile,
    /// The execution location within the file.
    pub location: SourceLocation,
    /// Contiguous source lines ordered by line number.
    pub lines: Arc<[SourceLine]>,
}

/// Static information about a function in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    /// The function's session-scoped identifier.
    pub id: FunctionId,
    /// The source-level function name.
    pub name: Arc<str>,
    /// The linker-visible function name, when known.
    pub linkage_name: Option<Arc<str>>,
    /// The function's declaration location, when known.
    pub declaration: Option<SourceLocation>,
}

/// Describes whether a function instance is emitted out of line or inlined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeInstanceKind {
    /// A physical, independently callable function body.
    OutOfLine,
    /// A function body expanded at a source call site.
    Inline {
        /// The call expression in the containing instance, when described.
        call_site: Option<SourceLocation>,
    },
}

/// Explains how an entry address was selected for a concrete function instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryProvenance {
    /// The debug format supplied an explicit entry address.
    Explicit,
    /// A recommended source statement supplied the entry address.
    Statement,
    /// The first concrete address range supplied the entry address.
    RangeStart,
}

/// A concrete entry address suitable for a function breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakpointEntry {
    /// The entry address in the module image.
    pub address: ImageAddress,
    /// How the address was selected.
    pub provenance: EntryProvenance,
}

/// One concrete placement of a source-level function in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeInstanceInfo {
    /// The instance's session-scoped identifier.
    pub id: CodeInstanceId,
    /// The source-level function represented by this instance.
    pub function: FunctionId,
    /// The nearest containing function instance, for an inline expansion.
    pub parent: Option<CodeInstanceId>,
    /// Whether the instance is physical or inlined.
    pub kind: CodeInstanceKind,
    /// Every image-address range occupied by the instance.
    pub ranges: Arc<[AddressRange<ImageAddress>]>,
    /// The preferred location for a function breakpoint, when one exists.
    pub breakpoint_entry: Option<BreakpointEntry>,
}

impl CodeInstanceInfo {
    /// Returns whether the instance contains an image address.
    #[must_use]
    pub fn contains(&self, address: ImageAddress) -> bool {
        self.ranges.iter().any(|range| range.contains(address))
    }
}

/// A linker symbol exported by a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    /// The symbol's session-scoped identifier.
    pub id: SymbolId,
    /// The linker-visible symbol name.
    pub name: Arc<str>,
    /// The symbol's image address.
    pub address: ImageAddress,
}

/// A resolved source and function location in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLocation {
    /// The address that was resolved.
    pub address: ImageAddress,
    /// The containing function, when known.
    pub function: Option<FunctionInfo>,
    /// The physical code instance containing the address, when known.
    pub physical_instance: Option<CodeInstanceId>,
    /// Active inline frames at this address.
    pub inline_frames: InlineFrameLookup,
    /// The corresponding source location, when known.
    pub source: Option<SourceLocation>,
}

/// An ordered set of active inline instances, from outermost to innermost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineChain {
    /// Concrete inline instances in logical call order.
    pub instances: Arc<[CodeInstanceId]>,
}

/// The result of resolving inline frames at one image address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineFrameLookup {
    /// No presentable inline frame is active.
    None,
    /// Exactly one logical inline chain is active.
    Unique(InlineChain),
    /// The debug metadata describes incompatible active chains.
    Ambiguous(Arc<[InlineChain]>),
}

/// The address space in which a breakpoint was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BreakpointLocation {
    /// A location relative to an immutable module image.
    Image(ImageAddress),
    /// An absolute location in a running process.
    Virtual(VirtualAddress),
}

/// A resolved execution location in a running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLocation {
    /// The loaded module containing the address.
    pub module: ModuleId,
    /// The process virtual address.
    pub address: VirtualAddress,
    /// Static metadata resolved from the corresponding module image.
    pub image: ImageLocation,
}

/// Describes how a stack frame was reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A normal machine-code activation.
    Physical,
    /// A source-level inline expansion within a physical activation.
    Inline,
    /// A signal trampoline activation.
    Signal,
}

/// A platform-independent stack frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// The frame's identifier within the current stop revision.
    pub id: StackFrameId,
    /// Zero-based position, beginning with the stopped frame.
    pub level: u32,
    /// How the frame was reconstructed.
    pub kind: FrameKind,
    /// The loaded module containing the instruction, when known.
    pub module: Option<ModuleId>,
    /// The exact instruction or resume address for the frame.
    pub instruction: VirtualAddress,
    /// The concrete code instance represented by the frame, when known.
    pub code_instance: Option<CodeInstanceId>,
    /// The containing function, when known.
    pub function: Option<FunctionInfo>,
    /// The corresponding source location, when known.
    pub source: Option<SourceLocation>,
}

pub struct FrameMetadata {
    pub code_instance: Option<CodeInstanceId>,
    pub function: Option<FunctionInfo>,
    pub source: Option<SourceLocation>,
}

impl StackFrame {
    pub(crate) fn new(
        level: u32,
        kind: FrameKind,
        module: Option<ModuleId>,
        instruction: VirtualAddress,
        location: Option<ImageLocation>,
    ) -> Self {
        let (function, source) = location.map_or((None, None), |location| {
            (location.function, location.source)
        });

        Self::from_parts(
            level,
            kind,
            module,
            instruction,
            FrameMetadata {
                code_instance: None,
                function,
                source,
            },
        )
    }

    pub(crate) fn from_parts(
        level: u32,
        kind: FrameKind,
        module: Option<ModuleId>,
        instruction: VirtualAddress,
        metadata: FrameMetadata,
    ) -> Self {
        Self {
            id: StackFrameId::new(level),
            level,
            kind,
            module,
            instruction,
            code_instance: metadata.code_instance,
            function: metadata.function,
            source: metadata.source,
        }
    }
}

/// Explains why a backtrace stopped growing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwindTermination {
    /// The unwind metadata declared that no caller exists.
    Complete,
    /// No unwind information covered the supplied instruction.
    NoUnwindInfo { address: VirtualAddress },
    /// The instruction could not be associated with a loaded module.
    ModuleNotFound { address: VirtualAddress },
    /// Valid metadata used a feature not implemented by this debugger.
    UnsupportedUnwindInfo { feature: Arc<str> },
    /// The unwind metadata was malformed.
    CorruptUnwindInfo { description: Arc<str> },
    /// A register required to reconstruct the caller was unavailable.
    RegisterUnavailable { register: Arc<str> },
    /// Inferior memory required by an unwind rule could not be read.
    MemoryReadFailed { address: VirtualAddress },
    /// The reconstructed caller did not make valid progress.
    InvalidCaller { description: Arc<str> },
    /// A previously visited frame state was encountered again.
    CycleDetected,
    /// The configured maximum frame count was reached.
    DepthLimit,
}

/// A backtrace and the reason its reconstruction ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backtrace {
    /// The thread whose stack was inspected.
    pub thread: ThreadId,
    /// Frames ordered from the stopped frame outward.
    pub frames: Arc<[StackFrame]>,
    /// The completion or failure reason for the trace.
    pub termination: UnwindTermination,
}

/// An internal image-address range associated with a source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEntry {
    pub range: AddressRange<ImageAddress>,
    pub location: SourceLocation,
}

/// One ordered row emitted by a source line program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementRow {
    /// The image address associated with this row.
    pub address: ImageAddress,
    /// The operation index for architectures with multiple operations per instruction.
    pub operation_index: u64,
    /// The corresponding source location.
    pub location: SourceLocation,
    /// The producer-defined discriminator for this source position.
    pub discriminator: u64,
    /// Semantic flags associated with the row.
    pub flags: StatementFlags,
    /// The instruction-set identifier supplied by the producer.
    pub isa: u64,
    /// The containing line-program sequence.
    pub sequence: LineSequenceId,
    /// The row's order within its sequence, including equal-address rows.
    pub ordinal: u32,
}

/// Semantic markers attached to one source line-program row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementFlags(u8);

impl StatementFlags {
    const IS_STATEMENT: u8 = 1 << 0;
    const BASIC_BLOCK: u8 = 1 << 1;
    const PROLOGUE_END: u8 = 1 << 2;
    const EPILOGUE_BEGIN: u8 = 1 << 3;

    pub(crate) const fn empty() -> Self {
        Self(0)
    }

    pub(crate) const fn with_statement(self, enabled: bool) -> Self {
        self.with(Self::IS_STATEMENT, enabled)
    }

    pub(crate) const fn with_basic_block(self, enabled: bool) -> Self {
        self.with(Self::BASIC_BLOCK, enabled)
    }

    pub(crate) const fn with_prologue_end(self, enabled: bool) -> Self {
        self.with(Self::PROLOGUE_END, enabled)
    }

    pub(crate) const fn with_epilogue_begin(self, enabled: bool) -> Self {
        self.with(Self::EPILOGUE_BEGIN, enabled)
    }

    const fn with(self, flag: u8, enabled: bool) -> Self {
        if enabled { Self(self.0 | flag) } else { self }
    }

    /// Returns whether the row is a recommended breakpoint location.
    #[must_use]
    pub const fn is_statement(self) -> bool {
        self.0 & Self::IS_STATEMENT != 0
    }

    /// Returns whether the row begins a basic block.
    #[must_use]
    pub const fn basic_block(self) -> bool {
        self.0 & Self::BASIC_BLOCK != 0
    }

    /// Returns whether the row marks the end of a function prologue.
    #[must_use]
    pub const fn prologue_end(self) -> bool {
        self.0 & Self::PROLOGUE_END != 0
    }

    /// Returns whether the row marks the beginning of a function epilogue.
    #[must_use]
    pub const fn epilogue_begin(self) -> bool {
        self.0 & Self::EPILOGUE_BEGIN != 0
    }
}

pub struct ModuleMetadata {
    pub functions: Vec<FunctionInfo>,
    pub code_instances: Vec<CodeInstanceInfo>,
    pub symbols: Vec<SymbolInfo>,
    pub source_files: Vec<SourceFile>,
    pub statements: Vec<StatementRow>,
    pub lines: Vec<LineEntry>,
}

#[derive(Debug)]
struct RangeIndexEntry<T> {
    start: u64,
    end: u64,
    prefix_max_end: u64,
    value: T,
}

#[derive(Debug)]
struct RangeIndex<T> {
    entries: Arc<[RangeIndexEntry<T>]>,
}

impl<T: Copy + Ord> RangeIndex<T> {
    fn new(entries: impl IntoIterator<Item = (AddressRange<ImageAddress>, T)>) -> Self {
        let mut entries = entries
            .into_iter()
            .map(|(range, value)| RangeIndexEntry {
                start: range.start.get(),
                end: range.end.get(),
                prefix_max_end: 0,
                value,
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| (entry.start, entry.end, entry.value));

        let mut prefix_max_end = 0;
        for entry in &mut entries {
            prefix_max_end = prefix_max_end.max(entry.end);
            entry.prefix_max_end = prefix_max_end;
        }

        Self {
            entries: entries.into(),
        }
    }

    fn containing(&self, address: ImageAddress) -> impl Iterator<Item = T> + '_ {
        let address = address.get();
        let mut index = self.entries.partition_point(|entry| entry.start <= address);

        std::iter::from_fn(move || {
            while index > 0 {
                index -= 1;
                let entry = &self.entries[index];
                if entry.prefix_max_end <= address {
                    return None;
                }
                if address < entry.end {
                    return Some(entry.value);
                }
            }

            None
        })
    }
}

struct ModuleIndexes {
    functions_by_name: BTreeMap<Arc<str>, Arc<[FunctionId]>>,
    symbols_by_name: BTreeMap<Arc<str>, Arc<[SymbolId]>>,
    instances_by_function: BTreeMap<FunctionId, Arc<[CodeInstanceId]>>,
    statements_by_source_line: BTreeMap<(SourceFileId, LineNumber), Arc<[ImageAddress]>>,
}

fn grouped_index<K: Ord, V>(entries: impl IntoIterator<Item = (K, V)>) -> BTreeMap<K, Arc<[V]>> {
    let mut grouped = BTreeMap::<K, Vec<V>>::new();
    for (key, value) in entries {
        grouped.entry(key).or_default().push(value);
    }

    grouped
        .into_iter()
        .map(|(key, values)| (key, values.into()))
        .collect()
}

fn build_module_indexes(metadata: &ModuleMetadata) -> ModuleIndexes {
    let functions_by_name = grouped_index(
        metadata
            .functions
            .iter()
            .map(|function| (Arc::clone(&function.name), function.id)),
    );
    let symbols_by_name = grouped_index(
        metadata
            .symbols
            .iter()
            .map(|symbol| (Arc::clone(&symbol.name), symbol.id)),
    );
    let instances_by_function = grouped_index(
        metadata
            .code_instances
            .iter()
            .map(|instance| (instance.function, instance.id)),
    );
    let mut statements_by_source_line =
        grouped_index(metadata.statements.iter().filter_map(|statement| {
            statement.flags.is_statement().then_some((
                (statement.location.file, statement.location.line),
                statement.address,
            ))
        }));
    for addresses in statements_by_source_line.values_mut() {
        let mut unique = addresses.to_vec();
        unique.sort_unstable();
        unique.dedup();
        *addresses = unique.into();
    }

    ModuleIndexes {
        functions_by_name,
        symbols_by_name,
        instances_by_function,
        statements_by_source_line,
    }
}

fn validate_dense_ids(metadata: &ModuleMetadata) {
    for (index, function) in metadata.functions.iter().enumerate() {
        assert_eq!(
            usize::try_from(function.id.0).expect("function ID fits usize"),
            index,
            "function IDs are dense and ordered"
        );
    }
    for (index, instance) in metadata.code_instances.iter().enumerate() {
        assert_eq!(
            usize::try_from(instance.id.0).expect("code instance ID fits usize"),
            index,
            "code instance IDs are dense and ordered"
        );
    }
    for (index, source_file) in metadata.source_files.iter().enumerate() {
        assert_eq!(
            usize::try_from(source_file.id.0).expect("source file ID fits usize"),
            index,
            "source file IDs are dense and ordered"
        );
    }
}

/// Immutable, normalized debug metadata for one executable module.
#[derive(Debug)]
pub struct ModuleImage {
    id: ModuleImageId,
    path: Arc<PathBuf>,
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    functions: Arc<[FunctionInfo]>,
    code_instances: Arc<[CodeInstanceInfo]>,
    symbols: Arc<[SymbolInfo]>,
    source_files: Arc<[SourceFile]>,
    statements: Arc<[StatementRow]>,
    lines: Arc<[LineEntry]>,
    functions_by_name: BTreeMap<Arc<str>, Arc<[FunctionId]>>,
    symbols_by_name: BTreeMap<Arc<str>, Arc<[SymbolId]>>,
    instances_by_function: BTreeMap<FunctionId, Arc<[CodeInstanceId]>>,
    statements_by_source_line: BTreeMap<(SourceFileId, LineNumber), Arc<[ImageAddress]>>,
    code_range_index: RangeIndex<CodeInstanceId>,
    line_range_index: RangeIndex<u32>,
}

impl ModuleImage {
    pub(crate) fn new(
        path: PathBuf,
        target: TargetDescription,
        address_range: AddressRange<ImageAddress>,
        metadata: ModuleMetadata,
    ) -> Self {
        validate_dense_ids(&metadata);
        let indexes = build_module_indexes(&metadata);
        let code_range_index =
            RangeIndex::new(metadata.code_instances.iter().flat_map(|instance| {
                instance
                    .ranges
                    .iter()
                    .copied()
                    .map(|range| (range, instance.id))
            }));
        let line_range_index =
            RangeIndex::new(metadata.lines.iter().enumerate().map(|(index, line)| {
                (
                    line.range,
                    u32::try_from(index).expect("line entry count fits u32"),
                )
            }));

        Self {
            id: ModuleImageId::new(0),
            path: Arc::new(path),
            target,
            address_range,
            functions: metadata.functions.into(),
            code_instances: metadata.code_instances.into(),
            symbols: metadata.symbols.into(),
            source_files: metadata.source_files.into(),
            statements: metadata.statements.into(),
            lines: metadata.lines.into(),
            functions_by_name: indexes.functions_by_name,
            symbols_by_name: indexes.symbols_by_name,
            instances_by_function: indexes.instances_by_function,
            statements_by_source_line: indexes.statements_by_source_line,
            code_range_index,
            line_range_index,
        }
    }

    /// Returns this image's session-scoped identifier.
    #[must_use]
    pub const fn id(&self) -> ModuleImageId {
        self.id
    }

    /// Returns the executable path used to load this image.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the properties of this image's target.
    #[must_use]
    pub const fn target(&self) -> TargetDescription {
        self.target
    }

    /// Returns whether an image address lies in this module's loadable range.
    #[must_use]
    pub fn contains_address(&self, address: ImageAddress) -> bool {
        self.address_range.contains(address)
    }

    /// Returns all functions described by this image.
    #[must_use]
    pub fn functions(&self) -> &[FunctionInfo] {
        &self.functions
    }

    /// Looks up a source-level function by identifier.
    #[must_use]
    pub fn function(&self, id: FunctionId) -> Option<&FunctionInfo> {
        self.functions.get(usize::try_from(id.0).ok()?)
    }

    /// Returns all concrete code instances described by this image.
    #[must_use]
    pub fn code_instances(&self) -> &[CodeInstanceInfo] {
        &self.code_instances
    }

    /// Returns all linker symbols described by this image.
    #[must_use]
    pub fn symbols(&self) -> &[SymbolInfo] {
        &self.symbols
    }

    /// Returns all source files referenced by this image.
    #[must_use]
    pub fn source_files(&self) -> &[SourceFile] {
        &self.source_files
    }

    /// Finds one source file using an absolute path or trailing path components.
    pub fn source_file_matching(&self, path: &Path) -> Result<&SourceFile> {
        let matches = self
            .source_files
            .iter()
            .filter(|source| path_matches(source.path.as_path(), path))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [source] => Ok(source),
            [] => Err(Error::SourceFileNotFound(path.to_path_buf())),
            _ => Err(Error::AmbiguousSourceFile {
                path: path.to_path_buf(),
                matches: matches
                    .iter()
                    .map(|source| source.path.as_ref().clone())
                    .collect(),
            }),
        }
    }

    /// Returns every ordered source line-program row in this image.
    #[must_use]
    pub fn statement_rows(&self) -> &[StatementRow] {
        &self.statements
    }

    pub(crate) fn line_entries(&self) -> &[LineEntry] {
        &self.lines
    }

    /// Looks up a concrete code instance by identifier.
    #[must_use]
    pub fn code_instance(&self, id: CodeInstanceId) -> Option<&CodeInstanceInfo> {
        self.code_instances.get(usize::try_from(id.0).ok()?)
    }

    /// Returns the concrete instances of one source-level function.
    pub fn instances_for_function(
        &self,
        function: FunctionId,
    ) -> impl Iterator<Item = &CodeInstanceInfo> {
        self.instances_by_function
            .get(&function)
            .into_iter()
            .flat_map(|instances| instances.iter())
            .filter_map(|instance| self.code_instance(*instance))
    }

    /// Returns image addresses associated with one source line.
    pub fn statement_addresses(
        &self,
        file: SourceFileId,
        line: LineNumber,
    ) -> impl Iterator<Item = ImageAddress> + '_ {
        self.statements_by_source_line
            .get(&(file, line))
            .into_iter()
            .flat_map(|addresses| addresses.iter())
            .copied()
    }

    /// Finds the single function with the supplied source-level name.
    pub fn function_named(&self, name: &str) -> Result<&FunctionInfo> {
        let matches = self
            .functions_by_name
            .get(name)
            .ok_or_else(|| Error::FunctionNotFound(name.to_owned()))?;
        let [function] = matches.as_ref() else {
            return Err(Error::DuplicateFunction(name.to_owned()));
        };

        Ok(self
            .function(*function)
            .expect("name index references a function"))
    }

    /// Finds the single linker symbol with the supplied name.
    pub fn symbol_named(&self, name: &str) -> Result<&SymbolInfo> {
        let matches = self
            .symbols_by_name
            .get(name)
            .ok_or_else(|| Error::SymbolNotFound(name.to_owned()))?;
        let [symbol] = matches.as_ref() else {
            return Err(Error::DuplicateSymbol(name.to_owned()));
        };

        Ok(self
            .symbols
            .iter()
            .find(|candidate| candidate.id == *symbol)
            .expect("name index references a symbol"))
    }

    /// Resolves an image address to its available function and source metadata.
    #[must_use]
    pub fn locate(&self, address: ImageAddress) -> ImageLocation {
        let physical = self
            .code_range_index
            .containing(address)
            .filter_map(|instance| self.code_instance(instance))
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
            .min_by_key(|instance| instance.id);
        let inline_frames = self.inline_frames(address, physical.map(|instance| instance.id));
        let logical_instance = match &inline_frames {
            InlineFrameLookup::Unique(chain) => chain.instances.last().copied(),
            InlineFrameLookup::None | InlineFrameLookup::Ambiguous(_) => None,
        };
        let function_id = logical_instance
            .and_then(|instance| self.code_instance(instance))
            .map(|instance| instance.function)
            .or_else(|| physical.map(|instance| instance.function));
        let function = function_id.and_then(|function_id| {
            self.functions
                .iter()
                .find(|function| function.id == function_id)
                .cloned()
        });
        let source = self
            .line_range_index
            .containing(address)
            .min()
            .and_then(|index| {
                self.lines
                    .get(usize::try_from(index).expect("u32 fits usize"))
            })
            .map(|entry| entry.location.clone());

        ImageLocation {
            address,
            function,
            physical_instance: physical.map(|instance| instance.id),
            inline_frames,
            source,
        }
    }

    fn inline_frames(
        &self,
        address: ImageAddress,
        physical: Option<CodeInstanceId>,
    ) -> InlineFrameLookup {
        let mut chains = Vec::new();

        for instance in self
            .code_range_index
            .containing(address)
            .filter_map(|instance| self.code_instance(instance))
            .filter(|instance| {
                matches!(
                    instance.kind,
                    CodeInstanceKind::Inline { call_site: Some(_) }
                )
            })
        {
            if let Some(chain) = self.inline_chain(instance.id, address, physical)
                && !chains.contains(&chain)
            {
                chains.push(chain);
            }
        }

        let chains: Vec<_> = chains
            .iter()
            .filter(|candidate| {
                !chains.iter().any(|other| {
                    candidate.len() < other.len() && other.starts_with(candidate.as_slice())
                })
            })
            .cloned()
            .map(|instances| InlineChain {
                instances: instances.into(),
            })
            .collect();

        match chains.len() {
            0 => InlineFrameLookup::None,
            1 => InlineFrameLookup::Unique(chains.into_iter().next().expect("one chain")),
            _ => InlineFrameLookup::Ambiguous(chains.into()),
        }
    }

    fn inline_chain(
        &self,
        mut instance: CodeInstanceId,
        address: ImageAddress,
        physical: Option<CodeInstanceId>,
    ) -> Option<Vec<CodeInstanceId>> {
        let mut chain = Vec::new();

        loop {
            let current = self.code_instance(instance)?;

            if !current.contains(address) {
                return None;
            }
            match &current.kind {
                CodeInstanceKind::Inline { call_site: Some(_) } => chain.push(current.id),
                CodeInstanceKind::Inline { call_site: None } => return None,
                CodeInstanceKind::OutOfLine => {
                    if Some(current.id) != physical {
                        return None;
                    }
                    break;
                }
            }
            instance = current.parent?;
        }

        chain.reverse();
        Some(chain)
    }

    /// Looks up a source file by its identifier.
    #[must_use]
    pub fn source_file(&self, id: SourceFileId) -> Option<&SourceFile> {
        self.source_files.get(usize::try_from(id.0).ok()?)
    }
}

fn path_matches(candidate: &Path, requested: &Path) -> bool {
    if requested.is_absolute() {
        return candidate == requested;
    }
    let candidate = candidate.components().collect::<Vec<_>>();
    let requested = requested.components().collect::<Vec<_>>();
    requested.len() <= candidate.len()
        && candidate[candidate.len() - requested.len()..] == requested[..]
}

/// A module image mapped into a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedModule {
    /// The loaded module's session-scoped identifier.
    pub id: ModuleId,
    /// The corresponding immutable module image.
    pub image: ModuleImageId,
    /// The load bias applied to image addresses.
    pub load_bias: u64,
}

impl LoadedModule {
    pub(crate) const fn main(image: ModuleImageId, load_bias: u64) -> Self {
        Self {
            id: ModuleId::new(0),
            image,
            load_bias,
        }
    }

    /// Converts an image address into a process virtual address.
    pub fn virtual_address(self, address: ImageAddress) -> Result<VirtualAddress> {
        self.load_bias
            .checked_add(address.get())
            .map(VirtualAddress::new)
            .ok_or(Error::AddressOverflow)
    }

    /// Converts a process virtual address into an image address.
    pub fn image_address(self, address: VirtualAddress) -> Result<ImageAddress> {
        address
            .get()
            .checked_sub(self.load_bias)
            .map(ImageAddress::new)
            .ok_or(Error::AddressOutsideModule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_path_matching_uses_whole_trailing_components() {
        let candidate = Path::new("/build/project/src/main.c");
        assert!(path_matches(candidate, Path::new("main.c")));
        assert!(path_matches(candidate, Path::new("src/main.c")));
        assert!(path_matches(candidate, candidate));
        assert!(!path_matches(candidate, Path::new("rc/main.c")));
        assert!(!path_matches(candidate, Path::new("other/main.c")));
    }

    fn source(line: u64) -> SourceLocation {
        SourceLocation {
            file: SourceFileId::new(0),
            line: LineNumber::new(line).expect("nonzero line"),
            column: None,
        }
    }

    fn instance(
        id: u32,
        function: u32,
        parent: Option<u32>,
        kind: CodeInstanceKind,
        ranges: &[(u64, u64)],
    ) -> CodeInstanceInfo {
        CodeInstanceInfo {
            id: CodeInstanceId::new(id),
            function: FunctionId::new(function),
            parent: parent.map(CodeInstanceId::new),
            kind,
            ranges: ranges
                .iter()
                .map(|&(start, end)| AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                })
                .collect::<Vec<_>>()
                .into(),
            breakpoint_entry: None,
        }
    }

    fn inline_test_image(code_instances: Vec<CodeInstanceInfo>) -> ModuleImage {
        let functions = ["physical", "middle", "leaf", "sibling"]
            .into_iter()
            .enumerate()
            .map(|(id, name)| FunctionInfo {
                id: FunctionId::new(u32::try_from(id).expect("small function count")),
                name: name.into(),
                linkage_name: None,
                declaration: None,
            })
            .collect();

        ModuleImage::new(
            PathBuf::from("/test/inline"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(100),
            },
            ModuleMetadata {
                functions,
                code_instances,
                symbols: Vec::new(),
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
            },
        )
    }

    #[test]
    fn loaded_module_translates_between_address_spaces_with_checked_arithmetic() {
        let module = LoadedModule::main(ModuleImageId::new(0), 0x4000);

        assert_eq!(
            module.virtual_address(ImageAddress::new(0x123)).unwrap(),
            VirtualAddress::new(0x4123)
        );
        assert_eq!(
            module.image_address(VirtualAddress::new(0x4123)).unwrap(),
            ImageAddress::new(0x123)
        );
        assert!(matches!(
            module.image_address(VirtualAddress::new(0x3fff)),
            Err(Error::AddressOutsideModule)
        ));
        assert!(matches!(
            module.virtual_address(ImageAddress::new(u64::MAX)),
            Err(Error::AddressOverflow)
        ));
    }

    #[test]
    fn inline_lookup_preserves_nested_discontiguous_ranges_and_boundaries() {
        let image = inline_test_image(vec![
            instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0, 100)]),
            instance(
                1,
                1,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(10)),
                },
                &[(20, 30), (40, 50)],
            ),
            instance(
                2,
                2,
                Some(1),
                CodeInstanceKind::Inline {
                    call_site: Some(source(20)),
                },
                &[(22, 25)],
            ),
            instance(
                3,
                3,
                Some(0),
                CodeInstanceKind::Inline { call_site: None },
                &[(60, 70)],
            ),
        ]);

        let InlineFrameLookup::Unique(nested) = image.locate(ImageAddress::new(22)).inline_frames
        else {
            panic!("nested inline chain was not unique")
        };
        assert_eq!(
            nested.instances.as_ref(),
            &[CodeInstanceId::new(1), CodeInstanceId::new(2)]
        );
        assert!(matches!(
            image.locate(ImageAddress::new(40)).inline_frames,
            InlineFrameLookup::Unique(_)
        ));
        for address in [30, 35, 50, 60] {
            assert_eq!(
                image.locate(ImageAddress::new(address)).inline_frames,
                InlineFrameLookup::None,
                "unexpected inline frame at {address}"
            );
        }
    }

    #[test]
    fn overlapping_sibling_inline_instances_are_explicitly_ambiguous() {
        let image = inline_test_image(vec![
            instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0, 100)]),
            instance(
                1,
                1,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(10)),
                },
                &[(20, 30)],
            ),
            instance(
                2,
                2,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(11)),
                },
                &[(25, 35)],
            ),
        ]);

        let InlineFrameLookup::Ambiguous(chains) =
            image.locate(ImageAddress::new(26)).inline_frames
        else {
            panic!("overlapping siblings were not reported as ambiguous")
        };
        assert_eq!(chains.len(), 2);
    }
}
