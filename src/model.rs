use std::collections::{BTreeMap, BTreeSet};
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
    GlobalVariableId,
    "Identifies a global variable within a module image."
);
id_type!(
    TypeId,
    "Identifies a normalized type within a module image."
);

impl GlobalVariableId {
    /// Returns the dense index within the containing module image.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TypeId {
    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}
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

/// Stable identity of one normalized type in a module image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeReference {
    /// The immutable image that owns the type.
    pub image: ModuleImageId,
    /// The type's dense identifier within that image.
    pub id: TypeId,
}

/// A source qualifier retained as an ordered type-graph node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeQualifier {
    /// C-family `const` qualification.
    Const,
    /// C-family `volatile` qualification.
    Volatile,
    /// C-family `restrict` qualification.
    Restrict,
    /// Atomic qualification.
    Atomic,
    /// Producer-defined immutable qualification.
    Immutable,
}

/// The source-level category represented by a DWARF reference type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReferenceKind {
    /// An lvalue reference.
    Lvalue,
    /// An rvalue reference.
    Rvalue,
}

/// The normalized shape of a debug type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeKind {
    /// A directly encoded scalar base type.
    Base(BaseType),
    /// A thin pointer. A missing target represents an unspecified pointee such as `void`.
    Pointer {
        /// The pointed-to type, when supplied by the producer.
        target: Option<TypeReference>,
        /// The target-specific DWARF address class; zero is the default class.
        address_class: u64,
    },
    /// A language reference represented by an address-like value.
    Reference {
        /// Whether this is an lvalue or rvalue reference.
        kind: ReferenceKind,
        /// The referred-to type.
        target: TypeReference,
        /// The target-specific DWARF address class; zero is the default class.
        address_class: u64,
    },
    /// An ordered qualifier around another type.
    Qualified {
        /// The qualifier at this graph node.
        qualifier: TypeQualifier,
        /// The qualified type.
        target: TypeReference,
    },
    /// A source alias around another type.
    Alias {
        /// The aliased type.
        target: TypeReference,
    },
    /// A deliberately unspecified type such as C `void`.
    Unspecified,
    /// A valid type whose value shape is not implemented yet.
    Opaque {
        /// A stable description of the unsupported DWARF type tag.
        description: Arc<str>,
    },
}

/// Immutable, normalized metadata for one type-graph node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo {
    /// Stable identity within the owning module image.
    pub reference: TypeReference,
    /// Producer name or a deterministic structural presentation.
    pub name: Arc<str>,
    /// Storage size in bytes, when known.
    pub byte_size: Option<u64>,
    /// The node's normalized shape.
    pub kind: TypeKind,
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
    /// A source-language truth value.
    Boolean(bool),
    /// A sign-extended integer.
    Signed(i128),
    /// An unsigned integer.
    Unsigned(u128),
    /// A binary floating-point value retained as exact target bits.
    Floating(FloatValue),
}

/// A decoded thin pointer or reference representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressValue {
    /// The target virtual address represented by the value.
    pub address: VirtualAddress,
}

/// A decoded variable or dereferenced value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValue {
    /// A supported scalar value.
    Scalar(ScalarValue),
    /// A concrete thin pointer or reference address.
    Address(AddressValue),
    /// An optimized pointer with no concrete address representation.
    ImplicitPointer,
}

/// How a variable's current value was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValueSource {
    /// Memory in the inferior's virtual address space.
    Memory(VirtualAddress),
    /// A target register containing the complete value.
    Register(RegisterDescriptor),
    /// Debug metadata supplies the value as a constant.
    Constant,
    /// A DWARF expression computes a value that has no storage location.
    Computed,
    /// Optimization retained a referent value but eliminated the pointer's address.
    ImplicitPointer,
}

/// Why an otherwise available pointer or reference cannot be dereferenced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DereferenceUnavailableReason {
    /// The pointer contains the null address.
    Null,
    /// The producer did not supply a concrete pointee type.
    UnspecifiedPointee,
    /// The pointee is a valid type whose value shape is not implemented.
    UnsupportedPointee(Arc<str>),
    /// The pointer uses a target address class the backend cannot interpret.
    AddressClass(u64),
    /// The referent could not be read or reconstructed at this stop.
    Unavailable(VariableUnavailableReason),
    /// The type metadata needed to dereference the value is defective.
    Malformed(VariableMalformedReason),
}

impl fmt::Display for DereferenceUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("cannot dereference a null pointer"),
            Self::UnspecifiedPointee => {
                formatter.write_str("the pointer has no concrete pointee type")
            }
            Self::UnsupportedPointee(description) => formatter.write_str(description),
            Self::AddressClass(class) => {
                write!(formatter, "address class {class} is unsupported")
            }
            Self::Unavailable(reason) => reason.fmt(formatter),
            Self::Malformed(reason) => write!(formatter, "malformed: {}", reason.description),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DereferenceTarget {
    Address(VirtualAddress),
    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },
}

/// Opaque capability for dereferencing one value from one exact stopped state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DereferenceReference {
    pub(crate) stop_id: crate::StopId,
    pub(crate) thread: ThreadId,
    pub(crate) module: ModuleId,
    pub(crate) image: ModuleImageId,
    pub(crate) context_address: Option<ImageAddress>,
    pub(crate) target_type: TypeId,
    pub(crate) target: DereferenceTarget,
}

impl DereferenceReference {
    /// Returns the stopped snapshot that owns this capability.
    #[must_use]
    pub const fn stop_id(&self) -> crate::StopId {
        self.stop_id
    }

    /// Returns the thread whose frame context produced this capability.
    #[must_use]
    pub const fn thread(&self) -> ThreadId {
        self.thread
    }
}

/// Whether an inspected value can be explicitly dereferenced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DereferenceState {
    /// The value is not a pointer or reference.
    NotApplicable,
    /// Dereference is valid at the capability's exact stopped state.
    Available(DereferenceReference),
    /// The value is an indirection, but dereference is unavailable for a typed reason.
    Unavailable(DereferenceUnavailableReason),
}

/// A valid DWARF feature that variable inspection does not yet implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedVariableFeature {
    /// Reconstructing a value as it existed at function entry.
    EntryValue,
    /// Recovering a parameter from the caller's call-site metadata.
    ParameterReference,
    /// Evaluating a referenced DIE's location expression.
    CrossDieEvaluation,
    /// Resolving a thread-local storage address.
    Tls,
    /// Reading from a non-default target address space.
    AddressSpace,
    /// Combining multiple or partial location pieces.
    CompositeLocation,
    /// Resolving a pointer to an object that has no concrete location.
    ImplicitPointer,
    /// Reading WebAssembly execution state.
    WasmLocation,
    /// Reading a target register class not captured by this backend.
    RegisterClass,
    /// Applying a typed DWARF operation outside the supported scalar types.
    TypedValue,
}

/// Why valid variable metadata cannot produce a value at this stop.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableUnavailableReason {
    /// The call-frame information uses a CFA expression not yet supported.
    CfaExpression,
    /// The producer supplied no value or active location at this instruction.
    OptimizedOut,
    /// The value requires a valid feature outside the current implementation.
    Unsupported(UnsupportedVariableFeature),
    /// A required target register is unavailable.
    RegisterUnavailable(Arc<str>),
    /// The expression exceeded the debugger's bounded work limits.
    EvaluationLimit,
    /// Another explicit limitation or runtime failure.
    Other(Arc<str>),
}

impl fmt::Display for VariableUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CfaExpression => formatter.write_str("CFA expressions are unsupported"),
            Self::OptimizedOut => formatter.write_str("the value is optimized out"),
            Self::Unsupported(feature) => write!(formatter, "{feature:?} is unsupported"),
            Self::RegisterUnavailable(register) => {
                write!(formatter, "register {register} is unavailable")
            }
            Self::EvaluationLimit => {
                formatter.write_str("DWARF expression evaluation limit exceeded")
            }
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

impl From<UnsupportedVariableFeature> for VariableUnavailableReason {
    fn from(feature: UnsupportedVariableFeature) -> Self {
        Self::Unsupported(feature)
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
        /// How the bytes were obtained.
        source: VariableValueSource,
        /// Exact bytes in target byte order, including ABI padding. An optimized
        /// implicit pointer has no concrete byte representation.
        raw: Option<Arc<[u8]>>,
        /// The decoded value.
        value: VariableValue,
        /// Explicit lazy dereference state.
        dereference: DereferenceState,
    },
    /// Valid metadata does not provide a supported readable value here.
    Unavailable(VariableUnavailableReason),
    /// This entry's metadata is defective.
    Malformed(VariableMalformedReason),
}

/// The source-level role of a visible data object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableKind {
    /// A formal parameter of the selected function or inline instance.
    Parameter,
    /// A local variable declared within the selected function.
    Local,
    /// A data object with static storage described by a module image.
    Global,
}

/// The source visibility of a global data object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobalVariableVisibility {
    /// The producer marks the object as externally visible.
    External,
    /// The object is local to one compilation unit.
    CompilationUnit,
}

/// The static type state retained for a global catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobalVariableType {
    /// A normalized type whose top-level node is available.
    Resolved(TypeInfo),
    /// A valid type outside the current inspection contract.
    Unsupported(Arc<str>),
    /// Defective type metadata isolated to this entry.
    Malformed(VariableMalformedReason),
}

/// Immutable source metadata for one global data object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableInfo {
    /// The identifier within the containing module image.
    pub id: GlobalVariableId,
    /// The unqualified source name.
    pub name: Arc<str>,
    /// The producer-normalized source qualification.
    pub qualified_name: Arc<str>,
    /// The linker identity, when supplied by debug metadata.
    pub linkage_name: Option<Arc<str>>,
    /// The declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,
    /// The resolved scalar type or an explicit unsupported/malformed state.
    pub type_info: GlobalVariableType,
    /// Whether the object is external or compilation-unit local.
    pub visibility: GlobalVariableVisibility,
}

/// One structured candidate returned for an ambiguous global selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableCandidate {
    /// The candidate within its module image.
    pub id: GlobalVariableId,
    /// The canonical source qualification.
    pub qualified_name: Arc<str>,
    /// The resolved declaration path, when known.
    pub declaration_path: Option<Arc<PathBuf>>,
    /// The declaration location, when known.
    pub declaration: Option<SourceLocation>,
}

/// Stable identity of an evaluated global in a loaded module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalVariableReference {
    /// The runtime module mapping used for evaluation.
    pub module: ModuleId,
    /// The immutable image containing the catalog entry.
    pub image: ModuleImageId,
    /// The entry within that image.
    pub variable: GlobalVariableId,
}

/// One immutable global entry associated with its runtime module mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedGlobalVariableInfo {
    /// The module mapping when the image is currently loaded.
    pub module: Option<LoadedModule>,
    /// The immutable module image that owns this entry.
    pub image: ModuleImageId,
    /// The image-level global metadata.
    pub variable: GlobalVariableInfo,
}

/// One bounded page of global catalog metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariablePage {
    /// The debugger revision at which loaded modules were enumerated.
    pub revision: u64,
    /// The zero-based offset represented by this page.
    pub offset: u64,
    /// The total number of entries matching the filter.
    pub total: u64,
    /// Deterministically ordered entries in this page.
    pub variables: Arc<[LoadedGlobalVariableInfo]>,
}

/// One variable or parameter visible in the selected stopped frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// Whether this data object is a parameter or local variable.
    pub kind: VariableKind,
    /// Stable module identity for a global; absent for parameters and locals.
    pub global: Option<GlobalVariableReference>,
    /// The source-level variable name.
    pub name: Arc<str>,
    /// Its declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,
    /// Its resolved type, when valid and supported.
    pub type_info: Option<TypeInfo>,
    /// Its current availability and value.
    pub state: VariableState,
}

/// One value produced by explicitly dereferencing a pointer or reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DereferencedValue {
    /// The pointee type.
    pub type_info: TypeInfo,
    /// Its current availability and value.
    pub state: VariableState,
}

/// Variables inspected from one logical frame of a stopped thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableSnapshot {
    /// The debugger revision at which the values were read.
    pub revision: u64,
    /// The stopped snapshot that authorized the reads.
    pub stop_id: crate::StopId,
    /// The thread whose selected logical frame was inspected.
    pub thread: ThreadId,
    /// The logical frame whose source scope selected these variables.
    pub frame: crate::PresentedFrame,
    /// Target data representation used for decoding.
    pub target: TargetDescription,
    /// Visible parameters followed by local variables in source declaration order.
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
    /// A recommended line-program boundary supplied the entry address.
    Statement,
    /// Target-specific instruction analysis proved a post-prologue address.
    AnalyzedPrologue,
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
    /// Whether the row that opened this range is a recommended breakpoint
    /// location (`is_stmt`). Source stepping stops only at statement rows.
    pub statement: bool,
}

/// One ordered row emitted by a source line program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementRow {
    /// The image address associated with this row.
    pub address: ImageAddress,
    /// The operation index for architectures with multiple operations per instruction.
    pub operation_index: u64,
    /// The corresponding source location, when the producer supplied one that
    /// can be resolved.
    ///
    /// Control-flow markers remain meaningful on rows without source
    /// attribution, including rows whose line is zero.
    pub location: Option<SourceLocation>,
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
    pub globals: Vec<GlobalVariableInfo>,
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
    globals_by_selector: BTreeMap<Arc<str>, Arc<[GlobalVariableId]>>,
    instances_by_function: BTreeMap<FunctionId, Arc<[CodeInstanceId]>>,
    statements_by_source_line: BTreeMap<(SourceFileId, LineNumber), Arc<[ImageAddress]>>,
    control_boundaries_by_address: BTreeMap<ImageAddress, Arc<[u32]>>,
    control_boundaries_by_instance: BTreeMap<CodeInstanceId, Arc<[u32]>>,
    recommended_entries_by_instance: BTreeMap<CodeInstanceId, Arc<[BreakpointEntry]>>,
}

struct ControlBoundaryIndexes {
    by_address: BTreeMap<ImageAddress, Arc<[u32]>>,
    by_instance: BTreeMap<CodeInstanceId, Arc<[u32]>>,
    recommended_entries: BTreeMap<CodeInstanceId, Arc<[BreakpointEntry]>>,
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

fn build_module_indexes(
    metadata: &ModuleMetadata,
    code_range_index: &RangeIndex<CodeInstanceId>,
) -> ModuleIndexes {
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
    let mut global_selectors = Vec::new();
    for global in &metadata.globals {
        global_selectors.push((Arc::clone(&global.name), global.id));
        if global.qualified_name != global.name {
            global_selectors.push((Arc::clone(&global.qualified_name), global.id));
        }
        if let Some(linkage_name) = &global.linkage_name {
            global_selectors.push((Arc::clone(linkage_name), global.id));
        }
        if let Some(declaration) = &global.declaration
            && let Some(source) = metadata
                .source_files
                .get(usize::try_from(declaration.file.0).expect("source file ID fits usize"))
        {
            let path = source.path.to_string_lossy();
            global_selectors.push((
                Arc::from(format!("{path}::{}", global.qualified_name)),
                global.id,
            ));
            if let Some(file_name) = source.path.file_name() {
                global_selectors.push((
                    Arc::from(format!(
                        "{}::{}",
                        file_name.to_string_lossy(),
                        global.qualified_name
                    )),
                    global.id,
                ));
            }
        }
    }
    let mut globals_by_selector = grouped_index(global_selectors);
    for ids in globals_by_selector.values_mut() {
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        *ids = unique.into();
    }
    let instances_by_function = grouped_index(
        metadata
            .code_instances
            .iter()
            .map(|instance| (instance.function, instance.id)),
    );
    let mut statements_by_source_line =
        grouped_index(metadata.statements.iter().filter_map(|statement| {
            let location = statement.location.as_ref()?;
            statement
                .flags
                .is_statement()
                .then_some(((location.file, location.line), statement.address))
        }));
    for addresses in statements_by_source_line.values_mut() {
        let mut unique = addresses.to_vec();
        unique.sort_unstable();
        unique.dedup();
        *addresses = unique.into();
    }

    let control_boundaries = build_control_boundary_indexes(metadata, code_range_index);

    ModuleIndexes {
        functions_by_name,
        symbols_by_name,
        globals_by_selector,
        instances_by_function,
        statements_by_source_line,
        control_boundaries_by_address: control_boundaries.by_address,
        control_boundaries_by_instance: control_boundaries.by_instance,
        recommended_entries_by_instance: control_boundaries.recommended_entries,
    }
}

fn build_control_boundary_indexes(
    metadata: &ModuleMetadata,
    code_range_index: &RangeIndex<CodeInstanceId>,
) -> ControlBoundaryIndexes {
    let by_address = grouped_index(
        metadata
            .statements
            .iter()
            .enumerate()
            .filter(|(_, row)| row.flags.prologue_end() || row.flags.epilogue_begin())
            .map(|(index, row)| {
                (
                    row.address,
                    u32::try_from(index).expect("line-program row count fits u32"),
                )
            }),
    );
    let mut by_instance = grouped_index(
        metadata
            .statements
            .iter()
            .enumerate()
            .filter(|(_, row)| row.flags.prologue_end() || row.flags.epilogue_begin())
            .flat_map(|(index, row)| {
                code_range_index
                    .containing(row.address)
                    .filter(|id| {
                        usize::try_from(id.0)
                            .ok()
                            .and_then(|index| metadata.code_instances.get(index))
                            .is_some_and(|instance| {
                                matches!(instance.kind, CodeInstanceKind::OutOfLine)
                            })
                    })
                    .map(move |id| {
                        (
                            id,
                            u32::try_from(index).expect("line-program row count fits u32"),
                        )
                    })
            }),
    );
    for rows in by_instance.values_mut() {
        let mut unique = rows.to_vec();
        let mut seen_rows = BTreeSet::new();
        unique.retain(|row| seen_rows.insert(*row));
        *rows = unique.into();
    }
    let recommended_entries_by_instance = metadata
        .code_instances
        .iter()
        .filter_map(|instance| {
            let mut entries = by_instance
                .get(&instance.id)
                .into_iter()
                .flat_map(|rows| rows.iter())
                .filter_map(|row| metadata.statements.get(*row as usize))
                .filter(|row| row.flags.prologue_end())
                .map(|row| BreakpointEntry {
                    address: row.address,
                    provenance: EntryProvenance::Statement,
                })
                .collect::<Vec<_>>();

            let mut seen_addresses = BTreeSet::new();
            entries.retain(|entry| seen_addresses.insert(entry.address));
            if entries.is_empty()
                && let Some(entry) = instance.breakpoint_entry
            {
                entries.push(entry);
            }
            (!entries.is_empty()).then_some((instance.id, Arc::from(entries)))
        })
        .collect();

    ControlBoundaryIndexes {
        by_address,
        by_instance,
        recommended_entries: recommended_entries_by_instance,
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
    for (index, global) in metadata.globals.iter().enumerate() {
        assert_eq!(
            usize::try_from(global.id.0).expect("global ID fits usize"),
            index,
            "global IDs are dense and ordered"
        );
    }
}

/// Immutable, normalized debug metadata for one ELF module image.
#[derive(Debug)]
pub struct ModuleImage {
    id: ModuleImageId,
    path: Arc<PathBuf>,
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    functions: Arc<[FunctionInfo]>,
    code_instances: Arc<[CodeInstanceInfo]>,
    symbols: Arc<[SymbolInfo]>,
    globals: Arc<[GlobalVariableInfo]>,
    source_files: Arc<[SourceFile]>,
    statements: Arc<[StatementRow]>,
    lines: Arc<[LineEntry]>,
    functions_by_name: BTreeMap<Arc<str>, Arc<[FunctionId]>>,
    symbols_by_name: BTreeMap<Arc<str>, Arc<[SymbolId]>>,
    globals_by_selector: BTreeMap<Arc<str>, Arc<[GlobalVariableId]>>,
    instances_by_function: BTreeMap<FunctionId, Arc<[CodeInstanceId]>>,
    statements_by_source_line: BTreeMap<(SourceFileId, LineNumber), Arc<[ImageAddress]>>,
    control_boundaries_by_address: BTreeMap<ImageAddress, Arc<[u32]>>,
    control_boundaries_by_instance: BTreeMap<CodeInstanceId, Arc<[u32]>>,
    recommended_entries_by_instance: BTreeMap<CodeInstanceId, Arc<[BreakpointEntry]>>,
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
        let code_range_index =
            RangeIndex::new(metadata.code_instances.iter().flat_map(|instance| {
                instance
                    .ranges
                    .iter()
                    .copied()
                    .map(|range| (range, instance.id))
            }));
        let indexes = build_module_indexes(&metadata, &code_range_index);
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
            globals: metadata.globals.into(),
            source_files: metadata.source_files.into(),
            statements: metadata.statements.into(),
            lines: metadata.lines.into(),
            functions_by_name: indexes.functions_by_name,
            symbols_by_name: indexes.symbols_by_name,
            globals_by_selector: indexes.globals_by_selector,
            instances_by_function: indexes.instances_by_function,
            statements_by_source_line: indexes.statements_by_source_line,
            control_boundaries_by_address: indexes.control_boundaries_by_address,
            control_boundaries_by_instance: indexes.control_boundaries_by_instance,
            recommended_entries_by_instance: indexes.recommended_entries_by_instance,
            code_range_index,
            line_range_index,
        }
    }

    pub(crate) const fn with_id(mut self, id: ModuleImageId) -> Self {
        self.id = id;
        self
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

    /// Returns every global catalog entry in deterministic source order.
    #[must_use]
    pub fn globals(&self) -> &[GlobalVariableInfo] {
        &self.globals
    }

    /// Looks up a global catalog entry by identifier.
    #[must_use]
    pub fn global(&self, id: GlobalVariableId) -> Option<&GlobalVariableInfo> {
        self.globals.get(usize::try_from(id.0).ok()?)
    }

    /// Resolves a basename, canonical qualification, source qualification, or
    /// linkage identity to exactly one catalog entry.
    pub fn global_named(&self, selector: &str) -> Result<&GlobalVariableInfo> {
        let matches = self
            .globals_by_selector
            .get(selector)
            .ok_or_else(|| Error::VariableNotFound(selector.to_owned()))?;
        let [id] = matches.as_ref() else {
            return Err(Error::AmbiguousGlobalVariable {
                selector: selector.to_owned(),
                candidates: matches
                    .iter()
                    .filter_map(|id| self.global(*id))
                    .map(|global| crate::GlobalVariableCandidate {
                        id: global.id,
                        qualified_name: Arc::clone(&global.qualified_name),
                        declaration_path: global
                            .declaration
                            .as_ref()
                            .and_then(|declaration| self.source_file(declaration.file))
                            .map(|source| Arc::clone(&source.path)),
                        declaration: global.declaration.clone(),
                    })
                    .collect(),
            });
        };
        Ok(self
            .global(*id)
            .expect("global index references a catalog entry"))
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

    /// Returns exact line-program control boundaries at an image address.
    ///
    /// Equal-address rows remain distinct and retain their sequence and
    /// ordinal. This query does not infer an epilogue region after an
    /// `epilogue_begin` marker.
    pub fn control_boundaries_at(
        &self,
        address: ImageAddress,
    ) -> impl Iterator<Item = &StatementRow> {
        self.control_boundaries_by_address
            .get(&address)
            .into_iter()
            .flat_map(|rows| rows.iter())
            .filter_map(|row| self.statements.get(*row as usize))
    }

    /// Returns line-program control boundaries contained by one physical code
    /// instance.
    ///
    /// Inline instances deliberately have no independently inferred physical
    /// prologue or epilogue boundaries.
    pub fn control_boundaries_for_instance(
        &self,
        instance: CodeInstanceId,
    ) -> impl Iterator<Item = &StatementRow> {
        self.control_boundaries_by_instance
            .get(&instance)
            .into_iter()
            .flat_map(|rows| rows.iter())
            .filter_map(|row| self.statements.get(*row as usize))
    }

    /// Returns the recommended physical locations for entering one concrete
    /// function instance.
    ///
    /// Every applicable `prologue_end` row is returned for an out-of-line
    /// instance. When no such row exists, or when the instance is inline, the
    /// instance's existing singular entry semantics are retained.
    pub fn recommended_entries_for_instance(
        &self,
        instance: CodeInstanceId,
    ) -> impl Iterator<Item = BreakpointEntry> + '_ {
        self.recommended_entries_by_instance
            .get(&instance)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .copied()
    }

    pub(crate) fn line_entries(&self) -> &[LineEntry] {
        &self.lines
    }

    pub(crate) fn line_entry_containing(&self, address: ImageAddress) -> Option<&LineEntry> {
        self.line_range_index
            .containing(address)
            .min()
            .and_then(|index| {
                self.lines
                    .get(usize::try_from(index).expect("u32 fits usize"))
            })
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
            .line_entry_containing(address)
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

/// Public identity and path for one runtime module mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModuleRecord {
    /// The checked runtime mapping.
    pub module: LoadedModule,
    /// The canonical ELF image path.
    pub path: Arc<PathBuf>,
}

/// Immutable process-wide loaded-module registry snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModuleSnapshot {
    /// The debugger revision represented by this snapshot.
    pub revision: u64,
    /// Loaded modules ordered by module identifier.
    pub modules: Arc<[LoadedModuleRecord]>,
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

    fn global_test_image() -> ModuleImage {
        let scalar = || {
            let base = BaseType {
                name: "int".into(),
                base_name: "int".into(),
                encoding: BaseTypeEncoding::Signed,
                byte_size: 4,
            };
            GlobalVariableType::Resolved(TypeInfo {
                reference: TypeReference {
                    image: ModuleImageId::new(0),
                    id: TypeId::new(0),
                },
                name: "int".into(),
                byte_size: Some(4),
                kind: TypeKind::Base(base),
            })
        };
        let globals = [
            ("left::shared", "_ZL11left_shared", 0),
            ("right::shared", "_ZL12right_shared", 1),
        ]
        .into_iter()
        .enumerate()
        .map(
            |(id, (qualified_name, linkage_name, file))| GlobalVariableInfo {
                id: GlobalVariableId::new(u32::try_from(id).expect("small global count")),
                name: "shared".into(),
                qualified_name: qualified_name.into(),
                linkage_name: Some(linkage_name.into()),
                declaration: Some(SourceLocation {
                    file: SourceFileId::new(file),
                    line: LineNumber::new(7).expect("nonzero line"),
                    column: None,
                }),
                type_info: scalar(),
                visibility: GlobalVariableVisibility::CompilationUnit,
            },
        )
        .collect();
        ModuleImage::new(
            PathBuf::from("/test/globals"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(1),
            },
            ModuleMetadata {
                functions: Vec::new(),
                code_instances: Vec::new(),
                symbols: Vec::new(),
                globals,
                source_files: vec![
                    SourceFile {
                        id: SourceFileId::new(0),
                        path: Arc::new(PathBuf::from("/build/src/left.c")),
                    },
                    SourceFile {
                        id: SourceFileId::new(1),
                        path: Arc::new(PathBuf::from("/build/src/right.c")),
                    },
                ],
                statements: Vec::new(),
                lines: Vec::new(),
            },
        )
    }

    #[test]
    fn global_indexes_support_exact_qualification_and_structured_ambiguity() {
        let image = global_test_image();

        assert_eq!(
            image
                .global_named("left::shared")
                .expect("qualified global")
                .id,
            GlobalVariableId::new(0)
        );
        assert_eq!(
            image
                .global_named("right.c::right::shared")
                .expect("source-qualified global")
                .id,
            GlobalVariableId::new(1)
        );
        assert_eq!(
            image
                .global_named("_ZL11left_shared")
                .expect("linkage-qualified global")
                .id,
            GlobalVariableId::new(0)
        );
        let Error::AmbiguousGlobalVariable {
            selector,
            candidates,
        } = image
            .global_named("shared")
            .expect_err("ambiguous basename")
        else {
            panic!("unexpected ambiguity error");
        };
        assert_eq!(selector, "shared");
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.id, candidate.qualified_name.as_ref()))
                .collect::<Vec<_>>(),
            [
                (GlobalVariableId::new(0), "left::shared"),
                (GlobalVariableId::new(1), "right::shared"),
            ]
        );
    }

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
                globals: Vec::new(),
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
            },
        )
    }

    fn boundary_test_instances() -> Vec<CodeInstanceInfo> {
        let physical = CodeInstanceInfo {
            id: CodeInstanceId::new(0),
            function: FunctionId::new(0),
            parent: None,
            kind: CodeInstanceKind::OutOfLine,
            ranges: Arc::from([AddressRange {
                start: ImageAddress::new(0x10),
                end: ImageAddress::new(0x30),
            }]),
            breakpoint_entry: Some(BreakpointEntry {
                address: ImageAddress::new(0x10),
                provenance: EntryProvenance::Explicit,
            }),
        };
        let inline = CodeInstanceInfo {
            id: CodeInstanceId::new(1),
            function: FunctionId::new(1),
            parent: Some(physical.id),
            kind: CodeInstanceKind::Inline {
                call_site: Some(source(7)),
            },
            ranges: Arc::from([AddressRange {
                start: ImageAddress::new(0x14),
                end: ImageAddress::new(0x20),
            }]),
            breakpoint_entry: Some(BreakpointEntry {
                address: ImageAddress::new(0x14),
                provenance: EntryProvenance::RangeStart,
            }),
        };

        vec![physical, inline]
    }

    fn boundary_test_row(
        address: u64,
        location: Option<SourceLocation>,
        flags: StatementFlags,
        sequence: u32,
        ordinal: u32,
    ) -> StatementRow {
        StatementRow {
            address: ImageAddress::new(address),
            operation_index: 0,
            location,
            discriminator: 0,
            flags,
            isa: 0,
            sequence: LineSequenceId::new(sequence),
            ordinal,
        }
    }

    fn boundary_test_rows() -> Vec<StatementRow> {
        vec![
            boundary_test_row(
                0x14,
                None,
                StatementFlags::empty()
                    .with_statement(true)
                    .with_prologue_end(true),
                0,
                2,
            ),
            boundary_test_row(
                0x14,
                Some(source(8)),
                StatementFlags::empty().with_epilogue_begin(true),
                0,
                3,
            ),
            boundary_test_row(
                0x18,
                Some(source(9)),
                StatementFlags::empty().with_prologue_end(true),
                1,
                0,
            ),
            boundary_test_row(
                0x20,
                None,
                StatementFlags::empty().with_epilogue_begin(true),
                1,
                1,
            ),
        ]
    }

    fn control_boundary_test_image() -> ModuleImage {
        let functions = ["physical", "inline"]
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
            PathBuf::from("/test/boundaries"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(0x40),
            },
            ModuleMetadata {
                functions,
                code_instances: boundary_test_instances(),
                symbols: Vec::new(),
                globals: Vec::new(),
                source_files: Vec::new(),
                statements: boundary_test_rows(),
                lines: Vec::new(),
            },
        )
    }

    #[test]
    fn control_boundary_indexes_preserve_exact_rows_and_physical_instance_ownership() {
        let image = control_boundary_test_image();

        let exact = image
            .control_boundaries_at(ImageAddress::new(0x14))
            .collect::<Vec<_>>();
        assert_eq!(exact.len(), 2);
        assert_eq!(
            (exact[0].sequence, exact[0].ordinal),
            (LineSequenceId::new(0), 2)
        );
        assert_eq!(
            (exact[1].sequence, exact[1].ordinal),
            (LineSequenceId::new(0), 3)
        );
        assert!(exact[0].location.is_none());
        assert!(
            image
                .control_boundaries_at(ImageAddress::new(0x15))
                .next()
                .is_none()
        );

        assert_eq!(
            image
                .control_boundaries_for_instance(CodeInstanceId::new(0))
                .map(|row| row.address)
                .collect::<Vec<_>>(),
            [0x14, 0x14, 0x18, 0x20].map(ImageAddress::new)
        );
        assert!(
            image
                .control_boundaries_for_instance(CodeInstanceId::new(1))
                .next()
                .is_none()
        );
        assert_eq!(
            image
                .recommended_entries_for_instance(CodeInstanceId::new(0))
                .collect::<Vec<_>>(),
            vec![
                BreakpointEntry {
                    address: ImageAddress::new(0x14),
                    provenance: EntryProvenance::Statement,
                },
                BreakpointEntry {
                    address: ImageAddress::new(0x18),
                    provenance: EntryProvenance::Statement,
                },
            ]
        );
        assert_eq!(
            image
                .recommended_entries_for_instance(CodeInstanceId::new(1))
                .collect::<Vec<_>>(),
            vec![BreakpointEntry {
                address: ImageAddress::new(0x14),
                provenance: EntryProvenance::RangeStart,
            }]
        );
        assert!(
            image
                .statement_addresses(SourceFileId::new(0), LineNumber::new(8).unwrap())
                .next()
                .is_none()
        );
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
