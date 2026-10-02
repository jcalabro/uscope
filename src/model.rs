use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Error, Result};

macro_rules! address_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:#x})", stringify!($name), self.0)
            }
        }

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

    /// Returns whether the range contains no address.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.end <= self.start
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

            /// Returns the dense index within the owning collection.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }

            /// Returns the dense index as a `usize` for indexing.
            #[allow(dead_code, reason = "not every identifier indexes a slice")]
            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

/// Defines a public `u64` identifier with numeric conversions and `Display`.
macro_rules! numeric_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Creates an identifier from its numeric representation.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the numeric representation of this identifier.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
pub(crate) use numeric_id;

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
    SectionId,
    "Identifies an allocated section within a module image."
);
id_type!(
    GlobalVariableId,
    "Identifies a global variable within a module image."
);
id_type!(
    TypeId,
    "Identifies a normalized type within a module image."
);

id_type!(
    StackFrameId,
    "Identifies a frame of one thread's backtrace at one stop by its level."
);
id_type!(
    RegisterId,
    "Identifies a register within a target architecture."
);

impl StackFrameId {
    /// The innermost frame of every stopped thread, where execution stopped.
    pub const INNERMOST: Self = Self(0);
}

numeric_id!(
    ThreadId,
    "Identifies a thread within a debug session by its platform value."
);

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
    /// The register bytes in the target's byte order, or `None` in a caller's
    /// frame when a callee may have overwritten the register without saving
    /// it, so its value in that frame is unknown.
    pub bytes: Option<Arc<[u8]>>,
}

/// The general register set of one frame of a stopped thread at one
/// debugger revision.
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
    /// The meaningful low-order bits when the representation is narrower than its storage.
    pub bit_size: Option<u64>,
}

/// An exact integral value with producer-defined signedness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntegerValue {
    /// A signed integral value.
    Signed(i128),
    /// An unsigned integral value.
    Unsigned(u128),
}

/// Whether symbolic integer names came from a language enumeration or associated constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnumerationOrigin {
    /// A source-language enumeration represented by `DW_TAG_enumeration_type`.
    Language,
    /// Constants associated with a named integer type, as emitted by Go.
    NamedConstants,
}

/// One symbolic name and exact value in an enumeration-like type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enumerator {
    /// The producer/source name.
    pub name: Arc<str>,
    /// The exact signed or unsigned value.
    pub value: IntegerValue,
}

/// Stable identity of one normalized type in a module image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeReference {
    /// The immutable image that owns the type.
    pub image: ModuleImageId,
    /// The type's dense identifier within that image.
    pub id: TypeId,
}

/// A source modifier retained as an ordered type-graph node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeModifier {
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
    /// Packed representation.
    Packed,
    /// Producer-defined shared qualification.
    Shared,
}

/// The relationship between a named type and its representation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NamedTypeRelationship {
    /// A source-language synonym, such as a C or C++ typedef.
    Synonym,
    /// A source-language type with distinct identity, such as a Go definition.
    Distinct,
    /// A producer-created wrapper describing a language encoding.
    Encoding,
    /// The producer and language do not establish the relationship safely.
    Unspecified,
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

/// The source-level aggregate category represented by a record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordKind {
    /// A structure value.
    Struct,
    /// A class value.
    Class,
}

/// The aggregate storage category that owns a discriminated variant part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariantStorageKind {
    /// Structure storage.
    Struct,
    /// Class storage.
    Class,
    /// Union storage.
    Union,
}

/// Source visibility attached to a record member or base class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Accessibility {
    /// Public access.
    Public,
    /// Protected access.
    Protected,
    /// Private access.
    Private,
}

/// A normalized instance-member location within its containing record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordMemberLayout {
    /// A byte-aligned constant offset from the containing object.
    ByteOffset(u64),
    /// An exact bit range from the beginning of the containing object.
    BitRange {
        /// The first bit in the field.
        bit_offset: u64,
        /// The field width in bits.
        bit_size: u64,
    },
    /// A provider-owned computation requiring a concrete containing-object address.
    Runtime,
}

/// One instance member in a normalized record type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordMember {
    /// The source member name; anonymous members have no name.
    pub name: Option<Arc<str>>,
    /// The member's type.
    pub type_ref: TypeReference,
    /// Its location within a containing instance.
    pub layout: RecordMemberLayout,
    /// Its normalized source accessibility.
    pub accessibility: Accessibility,
    /// Whether the producer marked this member as compiler-generated.
    pub artificial: bool,
    /// Whether a producer such as Go marked this as an embedded field.
    pub embedded: bool,
    /// Its declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,
}

/// Whether a base-class subobject is virtual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BaseClassVirtuality {
    /// An ordinary non-virtual base.
    None,
    /// A virtual base subobject.
    Virtual,
}

/// One base-class subobject in a normalized class type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseClass {
    /// The base type.
    pub type_ref: TypeReference,
    /// Its location within the derived object.
    pub layout: RecordMemberLayout,
    /// Its normalized source accessibility.
    pub accessibility: Accessibility,
    /// Whether this is a virtual base.
    pub virtuality: BaseClassVirtuality,
}

/// One exact or inclusive-range selector for a discriminated variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariantSelector {
    /// One exact discriminator value.
    Value(IntegerValue),
    /// An inclusive discriminator range.
    Range {
        /// The first selected value.
        low: IntegerValue,
        /// The last selected value.
        high: IntegerValue,
    },
}

/// How a variant is selected by the discriminator.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariantSelection {
    /// The fallback when no explicit selector matches.
    Default,
    /// One or more exact values or inclusive ranges.
    Selectors(Arc<[VariantSelector]>),
}

/// A stored discriminator member or a tag type without runtime storage.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariantDiscriminant {
    /// A concrete member whose location supplies the runtime discriminator.
    Stored(RecordMember),
    /// A tag type is described but no discriminator field exists in storage.
    TagType(TypeReference),
}

/// One variant and the components selected with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// The variant DIE name, when supplied independently of its components.
    pub name: Option<Arc<str>>,
    /// Its discriminator selection rule.
    pub selection: VariantSelection,
    /// Components in producer/source order.
    pub members: Arc<[RecordMember]>,
}

/// The normalized shape of a debug type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeKind {
    /// A directly encoded scalar base type.
    Base(BaseType),
    /// An integral representation with ordered symbolic names.
    Enumeration {
        /// The normalized integral storage representation.
        representation: BaseType,
        /// The producer-supplied underlying type edge, when present.
        underlying: Option<TypeReference>,
        /// Symbolic names in producer/source order. Duplicate values are preserved.
        enumerators: Arc<[Enumerator]>,
        /// Whether this is a language enum or a named integer with associated constants.
        origin: EnumerationOrigin,
        /// Whether the producer marked enumerator names as scoped.
        scoped: bool,
    },
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
    /// A statically bounded array with one or more dimensions.
    Array {
        /// The element type.
        element: TypeReference,
        /// Dimensions in source order.
        dimensions: Arc<[ArrayDimension]>,
    },
    /// A language slice descriptor with a runtime element count.
    Slice {
        /// The slice element type.
        element: TypeReference,
        /// Whether the descriptor includes a capacity field.
        has_capacity: bool,
    },
    /// A structure or class with ordered instance members and base subobjects.
    Record {
        /// The source aggregate category.
        kind: RecordKind,
        /// Direct instance members in producer/source order.
        members: Arc<[RecordMember]>,
        /// Base-class subobjects in producer/source order.
        bases: Arc<[BaseClass]>,
        /// Whether this is a declaration without a complete layout.
        incomplete: bool,
    },
    /// Overlapping aggregate storage without a producer-described active member.
    Union {
        /// Alternative member interpretations in producer/source order.
        members: Arc<[RecordMember]>,
        /// Whether this is a declaration without a complete layout.
        incomplete: bool,
    },
    /// Aggregate storage whose active components are selected by a discriminator.
    Variant {
        /// The containing aggregate category.
        storage: VariantStorageKind,
        /// Ordinary members outside the variant part.
        common_members: Arc<[RecordMember]>,
        /// Base subobjects outside the variant part.
        bases: Arc<[BaseClass]>,
        /// Stored discriminator or tag-only metadata.
        discriminant: Box<VariantDiscriminant>,
        /// Variants in producer/source order.
        variants: Arc<[Variant]>,
        /// Whether the containing aggregate has no complete layout.
        incomplete: bool,
    },
    /// An ordered source modifier around another type.
    Modified {
        /// The modifier at this graph node.
        modifier: TypeModifier,
        /// The modified type.
        target: TypeReference,
    },
    /// A named type relationship described by a producer wrapper.
    Named {
        /// The representation target, absent for an incomplete declaration.
        target: Option<TypeReference>,
        /// The source or producer relationship to that target.
        relationship: NamedTypeRelationship,
    },
    /// A deliberately unspecified type such as C `void`.
    Unspecified,
    /// A valid type whose value shape is not implemented yet.
    Opaque {
        /// A stable description of the unsupported DWARF type tag.
        description: Arc<str>,
    },
}

/// One statically known array dimension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayDimension {
    /// The source lower bound.
    pub lower_bound: i128,
    /// The number of elements in this dimension.
    pub count: u64,
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

/// One finalized node in an image's immutable normalized type graph.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeNode {
    /// A type whose normalized metadata is available.
    Resolved(TypeInfo),
    /// A type DIE whose metadata is defective and cannot be interpreted safely.
    Malformed {
        /// Stable identity within the owning module image.
        reference: TypeReference,
        /// A stable description of the defect.
        description: Arc<str>,
    },
}

impl TypeNode {
    /// Returns this node's stable identity.
    #[must_use]
    pub const fn reference(&self) -> TypeReference {
        match self {
            Self::Resolved(info) => info.reference,
            Self::Malformed { reference, .. } => *reference,
        }
    }
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

/// A decoded variable, child, or dereferenced value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValue {
    /// A supported scalar value.
    Scalar(ScalarValue),
    /// An integral value retaining all exact symbolic matches.
    Enumeration {
        /// The exact target value.
        value: IntegerValue,
        /// Exact symbolic matches in producer/source order.
        matches: Arc<[Enumerator]>,
    },
    /// A concrete thin pointer or reference address.
    Address(AddressValue),
    /// An optimized pointer with no concrete address representation.
    ImplicitPointer,
    /// An array whose elements are available through explicit child pages.
    Array {
        /// The array dimensions.
        dimensions: Arc<[ArrayDimension]>,
    },
    /// A decoded language slice whose elements are available through child pages.
    Slice {
        /// Runtime length from the descriptor.
        length: u64,
        /// Runtime capacity when present in the descriptor.
        capacity: Option<u64>,
    },
    /// A structure or class whose bases and members are available through child pages.
    Record,
    /// Overlapping union storage whose interpretations are available through child pages.
    Union,
    /// A discriminated aggregate whose selected components are available through child pages.
    Variant {
        /// The decoded stored discriminator; absent for tagless single variants.
        discriminant: Option<IntegerValue>,
        /// The selected variant, or `None` when no selector matched.
        active: Option<Arc<Variant>>,
    },
}

/// How one lazily evaluated child relates to its parent value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueChildRelationship {
    /// One row-major array element and its source-language indices.
    ArrayElement {
        /// The zero-based row-major index.
        index: u64,
        /// One index per source dimension, including each declared lower bound.
        indices: Arc<[i128]>,
    },
    /// One runtime slice element.
    SliceElement {
        /// The zero-based runtime index.
        index: u64,
    },
    /// One direct record, union, common-variant, or active-variant member.
    Member(RecordMember),
    /// One base-class subobject.
    Base(BaseClass),
}

/// Which bounded resource prevented complete value materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionLimit {
    /// Maximum visible variables examined or returned.
    Variables,
    /// Maximum requested target bytes read.
    MemoryBytes,
    /// Maximum number of logical target-memory reads.
    MemoryReads,
    /// Maximum number of value nodes.
    ValueNodes,
    /// Maximum aggregate nesting depth.
    AggregateDepth,
    /// Maximum debug-expression work.
    ExpressionWork,
}

/// Resource ceilings for one logical inspection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectionLimits {
    /// Visible variables that may be examined or returned.
    pub variables: u64,
    /// Value nodes that may be materialized.
    pub value_nodes: u64,
    /// Deepest aggregate path or expansion level.
    pub aggregate_depth: u64,
    /// Logical target-memory reads.
    pub memory_reads: u64,
    /// Requested target-memory bytes.
    pub memory_bytes: u64,
    /// Conservatively reserved debug-expression work units.
    pub expression_work: u64,
}

impl Default for InspectionLimits {
    fn default() -> Self {
        Self {
            variables: 256,
            value_nodes: 512,
            aggregate_depth: 64,
            memory_reads: 64,
            memory_bytes: 1_024,
            expression_work: 5_120_000,
        }
    }
}

impl InspectionLimits {
    /// Returns the unused allowance after one completed operation.
    #[must_use]
    pub const fn remaining_after(self, usage: InspectionUsage) -> Self {
        Self {
            variables: self.variables.saturating_sub(usage.variables),
            value_nodes: self.value_nodes.saturating_sub(usage.value_nodes),
            aggregate_depth: self.aggregate_depth.saturating_sub(usage.aggregate_depth),
            memory_reads: self.memory_reads.saturating_sub(usage.memory_reads),
            memory_bytes: self.memory_bytes.saturating_sub(usage.memory_bytes),
            expression_work: self.expression_work.saturating_sub(usage.expression_work),
        }
    }
}

/// Resources consumed while completing one inspection operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InspectionUsage {
    /// Visible variables examined or returned.
    pub variables: u64,
    /// Value nodes materialized.
    pub value_nodes: u64,
    /// Deepest aggregate path or expansion level reached.
    pub aggregate_depth: u64,
    /// Logical target-memory reads attempted.
    pub memory_reads: u64,
    /// Target-memory bytes requested.
    pub memory_bytes: u64,
    /// Conservatively reserved debug-expression work units.
    pub expression_work: u64,
}

/// The exact attempted reservation that exhausted an inspection resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectionExhaustion {
    /// Resource that could not be reserved.
    pub resource: InspectionLimit,
    /// Configured ceiling for the resource.
    pub limit: u64,
    /// Amount completed before the failed reservation.
    pub used: u64,
    /// Additional amount requested by the failed operation.
    pub requested: u64,
}

/// Whether a bounded inspection operation completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionCompletion {
    /// The requested operation completed.
    Complete,
    /// Work stopped at a typed resource boundary.
    Truncated(InspectionExhaustion),
}

impl InspectionCompletion {
    /// Returns the typed exhaustion when this operation was truncated.
    #[must_use]
    pub const fn exhaustion(self) -> Option<InspectionExhaustion> {
        match self {
            Self::Complete => None,
            Self::Truncated(exhaustion) => Some(exhaustion),
        }
    }
}

/// Storage retained by an opaque child capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueStorage {
    /// Storage in the inferior's virtual address space.
    Memory(VirtualAddress),
    /// Bounded immutable bytes captured while evaluating the parent value.
    Bytes {
        source: VariableValueSource,
        raw: Arc<[u8]>,
        start: usize,
        end: usize,
        address: Option<VirtualAddress>,
    },
    /// A DWARF implicit pointer that must be resolved at the originating frame.
    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },
}

/// Opaque capability for expanding one aggregate at one exact stopped state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildrenReference {
    pub(crate) stop_id: crate::StopId,
    pub(crate) thread: ThreadId,
    pub(crate) frame: StackFrameId,
    pub(crate) module: ModuleId,
    pub(crate) image: ModuleImageId,
    pub(crate) context_address: Option<ImageAddress>,
    pub(crate) target_type: TypeId,
    pub(crate) storage: ValueStorage,
    pub(crate) total: u64,
    pub(crate) active_variant: Option<usize>,
}

impl ValueChildrenReference {
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

    /// Returns the deterministic number of children exposed by this capability.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }
}

/// Whether an available value exposes lazily evaluated children.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueChildren {
    /// Scalars, enumerations, pointers, and references have no structural children.
    NotApplicable,
    /// Children can be requested in arbitrary bounded pages.
    Available(Arc<ValueChildrenReference>),
    /// The aggregate header is valid but its children cannot be evaluated.
    Unavailable(VariableUnavailableReason),
}

/// One child returned from a bounded aggregate page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChild {
    /// Its stable relationship to the page's parent.
    pub relationship: ValueChildRelationship,
    /// Its source-facing normalized type.
    pub type_info: TypeInfo,
    /// Its current availability and decoded summary.
    pub state: VariableState,
}

/// Backwards-compatible name for child-page inspection completion.
pub type ValuePageCompletion = InspectionCompletion;

/// One immutable page of children evaluated at a stopped snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildPage {
    /// The stopped snapshot that authorized the reads.
    pub stop_id: crate::StopId,
    /// The zero-based child offset represented by this page.
    pub offset: u64,
    /// The deterministic number of children exposed by the parent.
    pub total: u64,
    /// Children represented in stable parent order.
    pub children: Arc<[ValueChild]>,
    /// Whether the requested interval completed.
    pub completion: ValuePageCompletion,
    /// Resources consumed while producing this page.
    pub usage: InspectionUsage,
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
    pub(crate) frame: StackFrameId,
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
    Unavailable {
        /// The dereferenced expression's type (the pointee), when it resolves.
        /// `None` when the producer supplied no concrete pointee type.
        pointee: Option<Box<TypeInfo>>,
        /// Why the dereference cannot be performed.
        reason: DereferenceUnavailableReason,
    },
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
    /// Selecting among simultaneous valid locations for one object.
    AlternativeLocations,
    /// Materializing a valid source type representation not modeled by uscope.
    TypeRepresentation,
    /// Evaluating a valid runtime aggregate layout not modeled by uscope.
    RuntimeAggregateLocation,
    /// Decoding a valid scalar representation not modeled by uscope.
    ScalarRepresentation,
}

impl fmt::Display for UnsupportedVariableFeature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EntryValue => "DWARF entry values",
            Self::ParameterReference => "DWARF parameter references",
            Self::CrossDieEvaluation => "cross-entry DWARF evaluation",
            Self::Tls => "thread-local storage",
            Self::AddressSpace => "non-default address spaces",
            Self::CompositeLocation => "composite DWARF locations",
            Self::ImplicitPointer => "DWARF implicit pointers",
            Self::WasmLocation => "WebAssembly locations",
            Self::RegisterClass => "the requested register class",
            Self::TypedValue => "the requested typed DWARF value",
            Self::AlternativeLocations => "simultaneous alternative DWARF locations",
            Self::TypeRepresentation => "the source type representation",
            Self::RuntimeAggregateLocation => "the runtime aggregate location",
            Self::ScalarRepresentation => "the scalar representation",
        })
    }
}

/// One unavailable bit range within a source value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueBitRange {
    /// Offset from the least-addressed byte of the value, in bits.
    pub offset: u64,
    /// Number of unavailable bits.
    pub size: u64,
}

/// Evidence that a source value has no complete executable representation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OptimizedOutReason {
    /// The defining entry supplies neither a location nor a constant.
    NoLocation,
    /// The selected location description is empty.
    EmptyLocation,
    /// Only the listed portions of a composite value are undefined.
    UndefinedPieces {
        /// Undefined destination ranges in source-value bit coordinates.
        ranges: Arc<[ValueBitRange]>,
    },
}

/// Why the selected frame cannot provide its call-frame address.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallFrameUnavailableReason {
    /// The instruction has no usable module-relative context.
    NoInstructionContext,
    /// Unwinding terminated without producing a CFA.
    UnwindTerminated(Arc<str>),
}

/// Why thread-local storage cannot be resolved for this value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsUnavailableReason {
    /// The module has no loader identity usable by the TLS provider.
    ModuleIdentityUnavailable,
    /// No compatible TLS provider is available.
    ProviderUnavailable,
    /// The provider could not resolve this thread's address.
    LookupFailed(Arc<str>),
}

/// Why a requested structural value operation cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueAccessUnavailableReason {
    /// A pointer type does not name a concrete pointee type.
    UnspecifiedPointee,
    /// A dereference was requested from a non-pointer, non-reference value.
    NotPointerOrReference,
    /// The pointer uses a target-specific address class.
    AddressClass(u64),
    /// Dereferencing a null pointer cannot produce a value.
    NullPointer,
    /// Address arithmetic exceeded the target address width.
    AddressOverflow,
    /// A runtime member expression requires a concrete containing-object address.
    NoConcreteObjectAddress,
    /// The requested bit-field representation is non-integral.
    NonIntegralBitField,
    /// The selected member belongs to a different active variant.
    InactiveVariant(Option<Arc<str>>),
    /// An implicit-pointer view falls outside its referenced source object.
    ImplicitPointerOutOfBounds {
        /// Signed byte offset into the referenced object.
        offset: i64,
        /// Number of bytes requested from that offset.
        size: u64,
        /// Size of the referenced object.
        containing_size: u64,
    },
}

/// Why valid variable metadata cannot produce a value at this stop.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableUnavailableReason {
    /// The source value has no complete executable representation.
    OptimizedOut(OptimizedOutReason),
    /// No location-list entry applies at the current instruction.
    UnavailableAtInstruction,
    /// Selecting the location requires an instruction context that is unavailable.
    NoInstructionContext,
    /// The value requires a valid feature outside the current implementation.
    Unsupported(UnsupportedVariableFeature),
    /// A required target-memory interval is not completely readable.
    MemoryInaccessible {
        /// First address requested for the typed value.
        address: VirtualAddress,
        /// Total requested byte count.
        requested: u64,
        /// Contiguous bytes successfully read before the failure.
        completed: u64,
        /// First inaccessible address.
        next_address: VirtualAddress,
    },
    /// A required target register is unavailable.
    RegisterUnavailable(Arc<str>),
    /// A caller frame's value of a register its callees may overwrite was
    /// not saved, so the register's value in that frame is unknown.
    RegisterNotSaved(Arc<str>),
    /// The selected frame cannot provide its call-frame address.
    CallFrameUnavailable(CallFrameUnavailableReason),
    /// Thread-local storage cannot be resolved for this value.
    TlsUnavailable(TlsUnavailableReason),
    /// A structural value operation cannot be completed.
    ValueAccess(ValueAccessUnavailableReason),
    /// The expression exceeded the debugger's bounded work limits.
    EvaluationLimit,
    /// A typed live-inspection resource was exhausted.
    InspectionLimit(InspectionExhaustion),
    /// A runtime-sized array or slice index is outside its current bounds.
    IndexOutOfBounds {
        /// Requested source index.
        index: i128,
        /// First valid source index.
        lower_bound: i128,
        /// Number of valid elements.
        count: u64,
    },
}

impl fmt::Display for VariableUnavailableReason {
    #[expect(
        clippy::too_many_lines,
        reason = "every public unavailable category has one stable user-facing diagnosis"
    )]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OptimizedOut(
                OptimizedOutReason::NoLocation | OptimizedOutReason::EmptyLocation,
            ) => formatter.write_str("the value is optimized out"),
            Self::OptimizedOut(OptimizedOutReason::UndefinedPieces { ranges }) => {
                formatter.write_str("the value is partially optimized out in ")?;
                for (index, range) in ranges.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(
                        formatter,
                        "bits {}..{}",
                        range.offset,
                        range.offset.saturating_add(range.size)
                    )?;
                }
                Ok(())
            }
            Self::UnavailableAtInstruction => {
                formatter.write_str("the value is unavailable at the current instruction")
            }
            Self::NoInstructionContext => {
                formatter.write_str("no instruction context is available to select the value")
            }
            Self::Unsupported(feature) => {
                write!(formatter, "unsupported variable feature: {feature}")
            }
            Self::MemoryInaccessible {
                address,
                requested,
                completed,
                next_address,
            } => write!(
                formatter,
                "memory is inaccessible at {next_address}; read {completed} of {requested} bytes from {address}"
            ),
            Self::RegisterUnavailable(register) => {
                write!(formatter, "register {register} is unavailable")
            }
            Self::RegisterNotSaved(register) => write!(
                formatter,
                "register {register} was not saved by the frame's callees"
            ),
            Self::CallFrameUnavailable(CallFrameUnavailableReason::NoInstructionContext) => {
                formatter.write_str("the instruction has no call-frame context")
            }
            Self::CallFrameUnavailable(CallFrameUnavailableReason::UnwindTerminated(reason)) => {
                write!(formatter, "the call-frame address is unavailable: {reason}")
            }
            Self::TlsUnavailable(TlsUnavailableReason::ModuleIdentityUnavailable) => {
                formatter.write_str("the module has no TLS loader identity")
            }
            Self::TlsUnavailable(TlsUnavailableReason::ProviderUnavailable) => {
                formatter.write_str("no compatible TLS provider is available")
            }
            Self::TlsUnavailable(TlsUnavailableReason::LookupFailed(reason)) => {
                write!(formatter, "TLS lookup failed: {reason}")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::UnspecifiedPointee) => {
                formatter.write_str("the pointer has no concrete pointee type")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NotPointerOrReference) => {
                formatter.write_str("the value is not a pointer or reference")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::AddressClass(class)) => {
                write!(formatter, "pointer address class {class} is unsupported")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NullPointer) => {
                formatter.write_str("cannot dereference a null pointer")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::AddressOverflow) => {
                formatter.write_str("value address arithmetic overflowed")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NoConcreteObjectAddress) => {
                formatter.write_str("the value has no concrete containing-object address")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NonIntegralBitField) => {
                formatter.write_str("non-integral bit-fields are unsupported")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::InactiveVariant(name)) => {
                if let Some(name) = name {
                    write!(formatter, "the member belongs to inactive variant '{name}'")
                } else {
                    formatter.write_str("the member belongs to an inactive variant")
                }
            }
            Self::ValueAccess(ValueAccessUnavailableReason::ImplicitPointerOutOfBounds {
                offset,
                size,
                containing_size,
            }) => write!(
                formatter,
                "implicit-pointer range at offset {offset} with size {size} is outside its {containing_size}-byte object"
            ),
            Self::EvaluationLimit => {
                formatter.write_str("DWARF expression evaluation limit exceeded")
            }
            Self::InspectionLimit(exhaustion) => write!(
                formatter,
                "{:?} limit {} exhausted after {} while requesting {}",
                exhaustion.resource, exhaustion.limit, exhaustion.used, exhaustion.requested
            ),
            Self::IndexOutOfBounds {
                index,
                lower_bound,
                count,
            } => write!(
                formatter,
                "index {index} is outside the source bounds starting at {lower_bound} with {count} elements"
            ),
        }
    }
}

impl From<UnsupportedVariableFeature> for VariableUnavailableReason {
    fn from(feature: UnsupportedVariableFeature) -> Self {
        Self::Unsupported(feature)
    }
}

/// Stable category for defective variable metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableMalformedKind {
    /// An attribute has an invalid form or value.
    InvalidAttribute,
    /// A metadata reference has no valid target.
    InvalidReference,
    /// The normalized type graph is inconsistent.
    InvalidTypeGraph,
    /// A location list violates its structural contract.
    InvalidLocationList,
    /// A location expression is malformed.
    InvalidExpression,
    /// Type or aggregate layout metadata is inconsistent.
    InconsistentLayout,
    /// A constant cannot inhabit its declared type.
    InvalidConstant,
}

/// Why one variable's debug metadata is defective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableMalformedReason {
    /// Machine-readable defect category.
    pub kind: VariableMalformedKind,
    /// A stable human-readable diagnosis.
    pub description: Arc<str>,
}

/// Why readable bytes are not a valid value of the declared source type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableInvalidReason {
    /// A Boolean contains a representation other than zero or one.
    BooleanRepresentation(u128),
}

impl fmt::Display for VariableInvalidReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BooleanRepresentation(value) => {
                write!(formatter, "invalid boolean representation {value}")
            }
        }
    }
}

/// The text a string value holds, read for display.
///
/// The bytes are the program's, which need not be UTF-8; presenting them is
/// the client's choice. A C string's bytes end before its terminating NUL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSummary {
    /// The bytes read, at most [`TextSummary::MAX_BYTES`].
    pub bytes: Arc<[u8]>,
    /// Whether every byte of the text was read.
    pub completion: TextCompletion,
}

impl TextSummary {
    /// The most bytes of text read for one value.
    pub const MAX_BYTES: usize = 256;
}

/// Whether a text summary holds all of its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCompletion {
    /// Every byte was read.
    Complete,
    /// More text follows the bytes read: `length` bytes in all, when the
    /// string records its length.
    Truncated { length: Option<u64> },
    /// The text continues into memory that could not be read.
    Unreadable { address: VirtualAddress },
}

/// The inspection state of one visible variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableState {
    /// The value was read and decoded exactly.
    Available {
        /// How the bytes were obtained.
        source: VariableValueSource,
        /// Exact bytes materialized for this value in target byte order. Aggregate
        /// summaries generally retain storage only in their opaque child capability.
        raw: Option<Arc<[u8]>>,
        /// The decoded scalar or aggregate summary.
        value: VariableValue,
        /// Explicit lazy dereference state.
        dereference: DereferenceState,
        /// Explicit lazy structural-child state.
        children: ValueChildren,
        /// The text the value holds, for values that are strings: a pointer
        /// to characters, a character array, or a language's string type.
        text: Option<Arc<TextSummary>>,
    },
    /// Valid metadata does not provide a supported readable value here.
    Unavailable(VariableUnavailableReason),
    /// Bytes were read exactly, but do not form a valid value of the declared type.
    Invalid {
        /// How the bytes were obtained.
        source: VariableValueSource,
        /// Exact invalid bytes in target byte order.
        raw: Arc<[u8]>,
        /// Why the representation is invalid.
        reason: VariableInvalidReason,
    },
    /// This entry's metadata is defective.
    Malformed(VariableMalformedReason),
}

/// One ordered operation in a bounded structural value expression.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValuePathStep {
    /// A source-level name. The controller resolves the longest initial run as
    /// the data-object root; later names select aggregate members.
    Named(String),
    /// One source-language array or slice index.
    Index(i128),
    /// One explicit pointer or reference dereference.
    Dereference,
}

/// One bounded, structural value expression evaluated at a stopped snapshot.
///
/// Dots embedded in source-level object names are preserved by the
/// controller's longest-prefix root lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueExpression {
    /// Operations in evaluation order, beginning with at least one name.
    pub steps: Arc<[ValuePathStep]>,
}

/// Renders the expression in source syntax, parenthesizing each dereference.
impl fmt::Display for ValueExpression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;

        let mut output = String::new();
        for step in self.steps.iter() {
            match step {
                ValuePathStep::Named(name) => {
                    if !output.is_empty() {
                        output.push('.');
                    }
                    output.push_str(name);
                }
                ValuePathStep::Index(index) => write!(output, "[{index}]")?,
                ValuePathStep::Dereference => output = format!("(*{output})"),
            }
        }
        formatter.write_str(&output)
    }
}

/// One half-open source-index range selected from a one-dimensional array or slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueIndexRange {
    /// First source index included in the page.
    pub start: i128,
    /// First source index excluded from the page.
    pub end: i128,
}

/// A parsed structural expression and its optional terminal range view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedValueExpression {
    /// The value or aggregate containing the terminal range.
    pub expression: ValueExpression,
    /// A terminal bounded range; absent when the expression selects one value.
    pub range: Option<ValueIndexRange>,
}

/// The terminal value produced by structural expression inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedValue {
    /// The resolved terminal type, when valid and supported.
    pub type_info: Option<TypeInfo>,
    /// The terminal value's current availability and decoded representation.
    pub state: VariableState,
    /// Whether the requested inspection completed.
    pub completion: InspectionCompletion,
    /// Resources consumed while producing this value.
    pub usage: InspectionUsage,
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
    /// Whether the dereference completed.
    pub completion: InspectionCompletion,
    /// Resources consumed while producing this value.
    pub usage: InspectionUsage,
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
    /// The backtrace frame that was inspected.
    pub stack_frame: StackFrameId,
    /// The logical frame whose source scope selected these variables.
    pub frame: crate::PresentedFrame,
    /// Target data representation used for decoding.
    pub target: TargetDescription,
    /// Visible parameters followed by local variables in source declaration order.
    pub variables: Arc<[Variable]>,
    /// Whether every selected variable was represented.
    pub completion: InspectionCompletion,
    /// Resources consumed while producing this snapshot.
    pub usage: InspectionUsage,
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

/// Why a stopped target-memory read could not continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryReadUnavailableReason {
    /// The target rejected access to the next requested address.
    Inaccessible,
}

impl fmt::Display for MemoryReadUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inaccessible => formatter.write_str("memory inaccessible"),
        }
    }
}

/// Whether a stopped target-memory read returned its complete requested range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryReadCompletion {
    /// Every requested byte was returned.
    Complete,
    /// A contiguous prefix was returned before target memory became unavailable.
    Incomplete {
        /// The first requested address not represented in `MemoryRead::bytes`.
        next_address: VirtualAddress,
        /// Why reading could not continue.
        reason: MemoryReadUnavailableReason,
    },
}

/// Immutable bytes read from one exact stopped target state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRead {
    /// The debugger revision at which the bytes were read.
    pub revision: u64,
    /// The stopped snapshot that authorized the read.
    pub stop_id: crate::StopId,
    /// Target properties relevant to address and byte presentation.
    pub target: TargetDescription,
    /// The first requested virtual address.
    pub address: VirtualAddress,
    /// The number of bytes requested.
    pub requested: u64,
    /// The readable contiguous prefix beginning at `address`.
    pub bytes: Arc<[u8]>,
    /// Whether every requested byte was returned.
    pub completion: MemoryReadCompletion,
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
    /// The source file the debug information names.
    pub file: SourceFile,
    /// The file that was read: the recorded path, or where a source path
    /// map found it.
    pub path: Arc<PathBuf>,
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

/// The kind of entity a linker symbol names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    /// Machine code entered by calling the symbol's address.
    Function,
    /// An indirect function's resolver, which selects the implementation
    /// that calls through the symbol reach.
    IndirectFunction,
    /// A data object.
    Data,
    /// The object file does not describe what the symbol names.
    Unknown,
}

/// The linkage visibility of a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolBinding {
    /// Visible to other objects and preferred during linking.
    Global,
    /// Visible to other objects but overridable by a global definition.
    Weak,
    /// Private to the object that defines it.
    Local,
}

/// Explains how the end of a code symbol's extent was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolExtentProvenance {
    /// The symbol table declared the symbol's size.
    Declared,
    /// The symbol declared no size, so its extent ends at the first evidence
    /// of other code: the next symbol, the next unwind-table function, or
    /// the end of its section.
    Inferred,
}

/// The machine code named by a code symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolExtent {
    /// The non-empty image-address range beginning at the symbol's address.
    pub range: AddressRange<ImageAddress>,
    /// How the end of the range was determined.
    pub provenance: SymbolExtentProvenance,
}

/// A linker symbol defined by a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    /// The symbol's session-scoped identifier.
    pub id: SymbolId,
    /// The linker-visible symbol name.
    pub name: Arc<str>,
    /// The symbol's image address.
    pub address: ImageAddress,
    /// What the symbol names.
    pub kind: SymbolKind,
    /// The symbol's linkage visibility.
    pub binding: SymbolBinding,
    /// Whether the module's dynamic symbol table exports the symbol.
    pub exported: bool,
    /// The code the symbol names, for a code symbol whose extent lies within
    /// one executable section.
    pub extent: Option<SymbolExtent>,
    /// The storage a data symbol names, for a data symbol defined in an
    /// allocated, non-thread-local section whose declared size fits within
    /// it. The range is empty for an unsized symbol, which names only its own
    /// address.
    pub storage: Option<AddressRange<ImageAddress>>,
}

/// Records which symbol tables a module image provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTableSources {
    /// Whether the image has a static symbol table.
    pub static_table: bool,
    /// Whether the image has a dynamic symbol table.
    pub dynamic_table: bool,
    /// The state of the image's embedded compressed symbol table.
    pub embedded_table: EmbeddedSymbolTable,
}

impl Default for SymbolTableSources {
    fn default() -> Self {
        Self {
            static_table: false,
            dynamic_table: false,
            embedded_table: EmbeddedSymbolTable::Absent,
        }
    }
}

/// The state of a symbol table embedded in compressed form (on ELF, the
/// `.gnu_debugdata` `MiniDebugInfo` section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedSymbolTable {
    /// The image embeds no symbol table.
    Absent,
    /// The embedded symbol table was read.
    Loaded,
    /// The embedded symbol table could not be read, so its symbols are absent.
    Unusable {
        /// Why the table could not be read.
        reason: Arc<str>,
    },
}

/// The symbol whose code extent or data storage contains an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    /// The symbol's identifier within its module image.
    pub symbol: SymbolId,
    /// The linker-visible symbol name.
    pub name: Arc<str>,
    /// What the symbol names.
    pub kind: SymbolKind,
    /// The distance from the symbol's address to the described address.
    pub offset: u64,
    /// How the end of the symbol's extent or storage was determined. An
    /// unsized data symbol named at its own address is `Inferred`.
    pub provenance: SymbolExtentProvenance,
}

impl SymbolLocation {
    /// Returns the source-level spelling of a Rust or C++ mangled name, or
    /// `None` when the name is not mangled in a recognized scheme.
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }
}

/// An allocated section of a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionInfo {
    /// The section's identifier within its module image.
    pub id: SectionId,
    /// The section name recorded by the object file.
    pub name: Arc<str>,
    /// The non-empty image-address range the section occupies.
    pub range: AddressRange<ImageAddress>,
    /// Whether the section holds machine code.
    pub executable: bool,
    /// Whether the section is writable at run time.
    pub writable: bool,
}

/// The section containing an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionLocation {
    /// The section's identifier within its module image.
    pub section: SectionId,
    /// The section name.
    pub name: Arc<str>,
    /// The distance from the section's start to the described address.
    pub offset: u64,
    /// Whether the section holds machine code.
    pub executable: bool,
}

/// What a module image's static metadata says about one image address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAddressDescription {
    /// The address that was described.
    pub address: ImageAddress,
    /// The allocated section containing the address, when the image records
    /// section headers.
    pub section: Option<SectionLocation>,
    /// The code symbol whose extent contains the address or, failing that,
    /// the data symbol whose declared storage contains it.
    pub symbol: Option<SymbolLocation>,
}

/// A process address resolved to the loaded module containing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleAddress {
    /// The loaded module containing the address.
    pub module: ModuleId,
    /// The path of the module's image.
    pub path: Arc<PathBuf>,
    /// The image's description of the corresponding image address.
    pub image: ImageAddressDescription,
}

/// A process address resolved against the loaded modules of one stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressDescription {
    /// The described process virtual address.
    pub address: VirtualAddress,
    /// The module containing the address, or `None` when no loaded module's
    /// image covers it.
    pub module: Option<ModuleAddress>,
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
    /// The code symbol containing the address, when known.
    pub symbol: Option<SymbolLocation>,
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
    /// The frame's instruction: where execution stopped in the innermost
    /// frame, or the return address of an outer frame's call.
    pub address: VirtualAddress,
    /// Static metadata resolved from the corresponding module image. An outer
    /// frame's is resolved one byte before its return address, inside the
    /// call it is making.
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
    /// The code symbol containing a physical or signal frame's code, with
    /// its offset measured to [`Self::instruction`]. Inline frames carry no
    /// symbol because they are source-level expansions within one.
    pub symbol: Option<SymbolLocation>,
}

pub struct FrameMetadata {
    pub code_instance: Option<CodeInstanceId>,
    pub function: Option<FunctionInfo>,
    pub source: Option<SourceLocation>,
    pub symbol: Option<SymbolLocation>,
}

impl StackFrame {
    pub(crate) fn new(
        level: u32,
        kind: FrameKind,
        module: Option<ModuleId>,
        instruction: VirtualAddress,
    ) -> Self {
        Self::from_parts(
            level,
            kind,
            module,
            instruction,
            FrameMetadata {
                code_instance: None,
                function: None,
                source: None,
                symbol: None,
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
            symbol: metadata.symbol,
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

impl fmt::Display for UnwindTermination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Complete => formatter.write_str("the outermost frame has no caller"),
            Self::NoUnwindInfo { address } => {
                write!(formatter, "no unwind information covers {address}")
            }
            Self::ModuleNotFound { address } => {
                write!(formatter, "no loaded module contains {address}")
            }
            Self::UnsupportedUnwindInfo { feature } => {
                write!(formatter, "unsupported unwind feature: {feature}")
            }
            Self::CorruptUnwindInfo { description } => {
                write!(formatter, "malformed unwind information: {description}")
            }
            Self::RegisterUnavailable { register } => {
                write!(formatter, "register {register} is unavailable")
            }
            Self::MemoryReadFailed { address } => {
                write!(formatter, "unwind memory is unreadable at {address}")
            }
            Self::InvalidCaller { description } => {
                write!(formatter, "invalid unwind caller: {description}")
            }
            Self::CycleDetected => formatter.write_str("unwind metadata produced a frame cycle"),
            Self::DepthLimit => formatter.write_str("unwind depth limit reached"),
        }
    }
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
    pub symbol_sources: SymbolTableSources,
    pub globals: Vec<GlobalVariableInfo>,
    pub types: Arc<[TypeNode]>,
    pub source_files: Vec<SourceFile>,
    pub statements: Vec<StatementRow>,
    pub lines: Vec<LineEntry>,
    pub sections: Vec<SectionInfo>,
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
            && let Some(source) = metadata.source_files.get(declaration.file.index())
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
                        metadata
                            .code_instances
                            .get(id.index())
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
            function.id.index(),
            index,
            "function IDs are dense and ordered"
        );
    }
    for (index, instance) in metadata.code_instances.iter().enumerate() {
        assert_eq!(
            instance.id.index(),
            index,
            "code instance IDs are dense and ordered"
        );
    }
    for (index, source_file) in metadata.source_files.iter().enumerate() {
        assert_eq!(
            source_file.id.index(),
            index,
            "source file IDs are dense and ordered"
        );
    }
    for (index, symbol) in metadata.symbols.iter().enumerate() {
        assert_eq!(symbol.id.index(), index, "symbol IDs are dense and ordered");
        if let Some(extent) = symbol.extent {
            assert!(
                matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::IndirectFunction
                ) && extent.range.start == symbol.address
                    && extent.range.start < extent.range.end,
                "symbol extents are non-empty code ranges beginning at the symbol"
            );
        }
        if let Some(storage) = symbol.storage {
            assert!(
                symbol.kind == SymbolKind::Data
                    && symbol.extent.is_none()
                    && storage.start == symbol.address
                    && storage.start <= storage.end,
                "symbol storage is a data range beginning at the symbol"
            );
        }
    }
    for (index, section) in metadata.sections.iter().enumerate() {
        assert_eq!(
            section.id.index(),
            index,
            "section IDs are dense and ordered"
        );
        assert!(
            section.range.start < section.range.end,
            "sections are non-empty"
        );
    }
    for (index, global) in metadata.globals.iter().enumerate() {
        assert_eq!(global.id.index(), index, "global IDs are dense and ordered");
    }
    for (index, node) in metadata.types.iter().enumerate() {
        assert_eq!(
            usize::try_from(node.reference().id.get()).expect("type ID fits usize"),
            index,
            "type IDs are dense and ordered"
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
    symbol_sources: SymbolTableSources,
    sections: Arc<[SectionInfo]>,
    globals: Arc<[GlobalVariableInfo]>,
    types: Arc<[TypeNode]>,
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
    symbol_range_index: RangeIndex<SymbolId>,
    storage_range_index: RangeIndex<SymbolId>,
    /// Unsized data symbols, each indexed by its one-byte address.
    unsized_data_index: RangeIndex<SymbolId>,
    section_range_index: RangeIndex<SectionId>,
    /// Known instruction starts in address order, one per address.
    instruction_starts: Arc<[(ImageAddress, crate::BoundaryEvidence)]>,
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
        let symbol_range_index = RangeIndex::new(
            metadata
                .symbols
                .iter()
                .filter_map(|symbol| Some((symbol.extent?.range, symbol.id))),
        );
        let storage_range_index = RangeIndex::new(
            metadata
                .symbols
                .iter()
                .filter_map(|symbol| Some((symbol.storage?, symbol.id))),
        );
        let unsized_data_index = RangeIndex::new(metadata.symbols.iter().filter_map(|symbol| {
            let storage = symbol.storage?;
            (storage.start == storage.end).then_some((
                AddressRange {
                    start: storage.start,
                    end: ImageAddress::new(storage.start.get().checked_add(1)?),
                },
                symbol.id,
            ))
        }));
        let instruction_starts = instruction_starts(&metadata);
        let section_range_index = RangeIndex::new(
            metadata
                .sections
                .iter()
                .map(|section| (section.range, section.id)),
        );

        Self {
            id: ModuleImageId::new(0),
            path: Arc::new(path),
            target,
            address_range,
            functions: metadata.functions.into(),
            code_instances: metadata.code_instances.into(),
            symbols: metadata.symbols.into(),
            symbol_sources: metadata.symbol_sources,
            sections: metadata.sections.into(),
            globals: metadata.globals.into(),
            types: metadata.types,
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
            symbol_range_index,
            storage_range_index,
            unsized_data_index,
            section_range_index,
            instruction_starts,
        }
    }

    pub(crate) fn with_id(mut self, id: ModuleImageId) -> Self {
        assert!(
            self.types.iter().all(|node| node.reference().image == id),
            "every type node is owned by its module image"
        );
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

    pub(crate) fn path_arc(&self) -> Arc<PathBuf> {
        Arc::clone(&self.path)
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
        self.functions.get(id.index())
    }

    /// Returns all concrete code instances described by this image.
    #[must_use]
    pub fn code_instances(&self) -> &[CodeInstanceInfo] {
        &self.code_instances
    }

    /// Returns all linker symbols described by this image, ordered by
    /// address and then name.
    #[must_use]
    pub fn symbols(&self) -> &[SymbolInfo] {
        &self.symbols
    }

    /// Looks up a linker symbol by identifier.
    #[must_use]
    pub fn symbol(&self, id: SymbolId) -> Option<&SymbolInfo> {
        self.symbols.get(id.index())
    }

    /// Returns the image's allocated sections, ordered by address.
    #[must_use]
    pub fn sections(&self) -> &[SectionInfo] {
        &self.sections
    }

    /// Looks up an allocated section by identifier.
    #[must_use]
    pub fn section(&self, id: SectionId) -> Option<&SectionInfo> {
        self.sections.get(id.index())
    }

    /// Finds the allocated section containing an image address. Should
    /// malformed sections overlap, the innermost one wins.
    #[must_use]
    pub fn section_containing(&self, address: ImageAddress) -> Option<&SectionInfo> {
        self.section_range_index
            .containing(address)
            .filter_map(|id| self.section(id))
            .min_by_key(|section| {
                (
                    std::cmp::Reverse(section.range.start),
                    section.range.end,
                    section.id,
                )
            })
    }

    /// Returns which symbol tables this image provided.
    #[must_use]
    pub const fn symbol_sources(&self) -> &SymbolTableSources {
        &self.symbol_sources
    }

    /// Finds the code symbol whose extent contains an image address.
    ///
    /// Declared extents take precedence over inferred ones. Among the
    /// remaining candidates the innermost extent wins, then a function over an
    /// indirect-function resolver, then global over weak over local binding,
    /// then an exported symbol, then the name with the fewest leading
    /// underscores, then the bytewise-smallest name. An address that no extent
    /// contains has no symbol; the nearest preceding symbol is never guessed.
    #[must_use]
    pub fn symbolize(&self, address: ImageAddress) -> Option<SymbolLocation> {
        let symbol = self
            .symbol_range_index
            .containing(address)
            .filter_map(|id| self.symbol(id))
            .min_by_key(|symbol| symbol_preference(symbol))?;
        let extent = symbol.extent.expect("indexed symbols have extents");

        Some(SymbolLocation {
            symbol: symbol.id,
            name: Arc::clone(&symbol.name),
            kind: symbol.kind,
            offset: address.get() - symbol.address.get(),
            provenance: extent.provenance,
        })
    }

    /// Finds the data symbol naming an image address: the one whose declared
    /// storage contains it, choosing among overlapping storage as
    /// [`Self::symbolize`] chooses among code extents, or otherwise an
    /// unsized data symbol at exactly that address. An address inside no
    /// declared storage is never attributed to the nearest preceding object.
    #[must_use]
    pub fn symbolize_data(&self, address: ImageAddress) -> Option<SymbolLocation> {
        let symbol = self
            .storage_range_index
            .containing(address)
            .filter_map(|id| self.symbol(id))
            .min_by_key(|symbol| storage_preference(symbol))
            .or_else(|| {
                self.unsized_data_index
                    .containing(address)
                    .filter_map(|id| self.symbol(id))
                    .min_by_key(|symbol| storage_preference(symbol))
            })?;
        let storage = symbol.storage.expect("indexed symbols have storage");

        Some(SymbolLocation {
            symbol: symbol.id,
            name: Arc::clone(&symbol.name),
            kind: symbol.kind,
            offset: address.get() - symbol.address.get(),
            provenance: if storage.start < storage.end {
                SymbolExtentProvenance::Declared
            } else {
                SymbolExtentProvenance::Inferred
            },
        })
    }

    /// Returns the addresses within `range` that are known to begin an
    /// instruction: the start of every range of an out-of-line function
    /// instance, of every code symbol with an extent, and of every
    /// executable section. Line
    /// table rows are deliberately excluded: some producers emit rows inside
    /// instructions, such as Go after a `LOCK` prefix.
    pub fn instruction_starts(
        &self,
        range: AddressRange<ImageAddress>,
    ) -> impl Iterator<Item = (ImageAddress, crate::BoundaryEvidence)> + '_ {
        let first = self
            .instruction_starts
            .partition_point(|(address, _)| *address < range.start);
        self.instruction_starts[first..]
            .iter()
            .take_while(move |(address, _)| *address < range.end)
            .copied()
    }

    /// Returns the source line containing an image address, when the line
    /// table describes it.
    #[must_use]
    pub fn source_location(&self, address: ImageAddress) -> Option<SourceLocation> {
        self.line_entry_containing(address)
            .map(|entry| entry.location.clone())
    }

    /// Describes an image address by its section and by the code symbol, or
    /// otherwise the data symbol, containing it.
    #[must_use]
    pub fn describe(&self, address: ImageAddress) -> ImageAddressDescription {
        ImageAddressDescription {
            address,
            section: self
                .section_containing(address)
                .map(|section| SectionLocation {
                    section: section.id,
                    name: Arc::clone(&section.name),
                    offset: address.get() - section.range.start.get(),
                    executable: section.executable,
                }),
            symbol: self
                .symbolize(address)
                .or_else(|| self.symbolize_data(address)),
        }
    }

    /// Returns every global catalog entry in deterministic source order.
    #[must_use]
    pub fn globals(&self) -> &[GlobalVariableInfo] {
        &self.globals
    }

    /// Looks up a global catalog entry by identifier.
    #[must_use]
    pub fn global(&self, id: GlobalVariableId) -> Option<&GlobalVariableInfo> {
        self.globals.get(id.index())
    }

    /// Returns the reachable, normalized type graph in stable identifier order.
    #[must_use]
    pub fn types(&self) -> &[TypeNode] {
        &self.types
    }

    /// Resolves a reference owned by this image to its finalized graph node.
    #[must_use]
    pub fn type_node(&self, reference: TypeReference) -> Option<&TypeNode> {
        if reference.image != self.id {
            return None;
        }
        self.types
            .get(reference.id.index())
            .filter(|node| node.reference() == reference)
    }

    /// Resolves a reference to normalized metadata when the node is not malformed.
    #[must_use]
    pub fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        match self.type_node(reference)? {
            TypeNode::Resolved(info) => Some(info),
            TypeNode::Malformed { .. } => None,
        }
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
        self.code_instances.get(id.index())
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

    /// Returns the lines of one source file within `lines` that have
    /// statement addresses: the lines a source breakpoint stops at as
    /// requested.
    pub fn breakpoint_lines(
        &self,
        file: SourceFileId,
        lines: std::ops::RangeInclusive<LineNumber>,
    ) -> impl Iterator<Item = LineNumber> + '_ {
        self.statements_by_source_line
            .range((file, *lines.start())..=(file, *lines.end()))
            .map(|((_, line), _)| *line)
    }

    /// Finds the line a source breakpoint requested at `line` stops at, as
    /// gdb does: the line itself when it has statements, otherwise the next
    /// line that does, provided a function whose statements begin at or
    /// before the request contains it. A line between functions never moves
    /// into the next one.
    #[must_use]
    pub fn breakpoint_line(&self, file: SourceFileId, line: LineNumber) -> Option<LineNumber> {
        let ((_, next), addresses) = self
            .statements_by_source_line
            .range((file, line)..)
            .next()
            .filter(|((next_file, _), _)| *next_file == file)?;
        if *next == line {
            return Some(line);
        }
        let encloses_request = |instance: &CodeInstanceInfo| {
            self.statements.iter().any(|row| {
                row.flags.is_statement()
                    && instance.contains(row.address)
                    && row
                        .location
                        .as_ref()
                        .is_some_and(|location| location.file == file && location.line <= line)
            })
        };
        addresses
            .iter()
            .flat_map(|address| {
                self.code_range_index
                    .containing(*address)
                    .filter_map(|instance| self.code_instance(instance))
            })
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
            .any(encloses_request)
            .then_some(*next)
    }

    /// Finds the single function with the supplied source-level name.
    ///
    /// Only functions with code compete: a compile unit that merely calls
    /// a function defined in another one may describe it by a declaration.
    pub fn function_named(&self, name: &str) -> Result<&FunctionInfo> {
        let named = self.functions_named(name).collect::<Vec<_>>();
        let defined = named
            .iter()
            .copied()
            .filter(|function| self.instances_for_function(function.id).next().is_some())
            .collect::<Vec<_>>();
        match (defined.as_slice(), named.as_slice()) {
            ([function], _) | ([], [function]) => Ok(function),
            (_, []) => Err(Error::FunctionNotFound(name.to_owned())),
            _ => Err(Error::DuplicateFunction(name.to_owned())),
        }
    }

    /// Returns every function with the supplied source-level name, such as
    /// C++ overloads and same-named static functions of different files.
    pub fn functions_named(&self, name: &str) -> impl Iterator<Item = &FunctionInfo> {
        self.functions_by_name
            .get(name)
            .into_iter()
            .flat_map(|functions| functions.iter())
            .map(|function| {
                self.function(*function)
                    .expect("name index references a function")
            })
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
            .symbol(*symbol)
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
        let function = function_id
            .and_then(|function_id| self.function(function_id))
            .cloned();
        let source = self
            .line_entry_containing(address)
            .map(|entry| entry.location.clone());

        ImageLocation {
            address,
            function,
            physical_instance: physical.map(|instance| instance.id),
            inline_frames,
            source,
            symbol: self.symbolize(address),
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
        self.source_files.get(id.index())
    }
}

/// Orders the code symbols containing one address from most to least
/// preferred; see [`ModuleImage::symbolize`].
fn symbol_preference(symbol: &SymbolInfo) -> impl Ord + '_ {
    let extent = symbol.extent.expect("indexed symbols have extents");
    (
        extent.provenance,
        std::cmp::Reverse(extent.range.start),
        extent.range.end.get() - extent.range.start.get(),
        symbol.kind,
        symbol.binding,
        !symbol.exported,
        symbol.name.bytes().take_while(|byte| *byte == b'_').count(),
        symbol.name.as_ref(),
        symbol.id,
    )
}

/// Collects the addresses debug information and code symbols prove begin
/// instructions, keeping the strongest evidence for each address.
fn instruction_starts(metadata: &ModuleMetadata) -> Arc<[(ImageAddress, crate::BoundaryEvidence)]> {
    let mut starts = BTreeMap::new();
    let functions = metadata
        .code_instances
        .iter()
        .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        .flat_map(|instance| instance.ranges.iter())
        .map(|range| (range.start, crate::BoundaryEvidence::FunctionRange));
    let symbols = metadata
        .symbols
        .iter()
        .filter_map(|symbol| symbol.extent)
        .map(|extent| (extent.range.start, crate::BoundaryEvidence::CodeSymbol));
    let sections = metadata
        .sections
        .iter()
        .filter(|section| section.executable)
        .map(|section| (section.range.start, crate::BoundaryEvidence::SectionStart));
    for (address, evidence) in functions.chain(symbols).chain(sections) {
        starts
            .entry(address)
            .and_modify(|current: &mut crate::BoundaryEvidence| *current = (*current).min(evidence))
            .or_insert(evidence);
    }
    starts.into_iter().collect()
}

/// Orders the data symbols naming one address, preferring the innermost
/// storage and then the names [`symbol_preference`] prefers.
fn storage_preference(symbol: &SymbolInfo) -> impl Ord + '_ {
    let storage = symbol.storage.expect("indexed symbols have storage");
    (
        std::cmp::Reverse(storage.start),
        storage.end.get() - storage.start.get(),
        symbol.binding,
        !symbol.exported,
        symbol.name.bytes().take_while(|byte| *byte == b'_').count(),
        symbol.name.as_ref(),
        symbol.id,
    )
}

/// Matches an absolute path exactly and a relative path as a suffix of whole
/// components, ignoring `.` components such as a leading `./`.
fn path_matches(candidate: &Path, requested: &Path) -> bool {
    fn significant(path: &Path) -> Vec<std::path::Component<'_>> {
        path.components()
            .filter(|component| *component != std::path::Component::CurDir)
            .collect()
    }

    if requested.is_absolute() {
        return candidate == requested;
    }
    let (candidate, requested) = (significant(candidate), significant(requested));
    !requested.is_empty() && candidate.ends_with(&requested)
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
                bit_size: None,
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
                symbol_sources: crate::model::SymbolTableSources::default(),
                globals,
                types: Arc::default(),
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
                sections: Vec::new(),
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
    fn module_type_graph_resolves_only_owned_dense_references() {
        let image_id = ModuleImageId::new(7);
        let resolved_reference = TypeReference {
            image: image_id,
            id: TypeId::new(0),
        };
        let malformed_reference = TypeReference {
            image: image_id,
            id: TypeId::new(1),
        };
        let image = ModuleImage::new(
            PathBuf::from("/test/types"),
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
                symbol_sources: crate::model::SymbolTableSources::default(),
                globals: Vec::new(),
                types: Arc::from([
                    TypeNode::Resolved(TypeInfo {
                        reference: resolved_reference,
                        name: "int".into(),
                        byte_size: Some(4),
                        kind: TypeKind::Opaque {
                            description: "test type".into(),
                        },
                    }),
                    TypeNode::Malformed {
                        reference: malformed_reference,
                        description: "bad type".into(),
                    },
                ]),
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
                sections: Vec::new(),
            },
        )
        .with_id(image_id);

        assert_eq!(image.types().len(), 2);
        assert!(matches!(
            image.type_node(resolved_reference),
            Some(TypeNode::Resolved(info)) if info.name.as_ref() == "int"
        ));
        assert_eq!(
            image
                .type_info(resolved_reference)
                .expect("resolved type")
                .name
                .as_ref(),
            "int"
        );
        assert!(matches!(
            image.type_node(malformed_reference),
            Some(TypeNode::Malformed { description, .. }) if description.as_ref() == "bad type"
        ));
        assert!(image.type_info(malformed_reference).is_none());
        assert!(
            image
                .type_node(TypeReference {
                    image: ModuleImageId::new(8),
                    id: TypeId::new(0),
                })
                .is_none()
        );
        assert!(
            image
                .type_node(TypeReference {
                    image: image_id,
                    id: TypeId::new(2),
                })
                .is_none()
        );
    }

    #[test]
    fn source_path_matching_uses_whole_trailing_components() {
        let candidate = Path::new("/build/project/src/main.c");
        assert!(path_matches(candidate, Path::new("main.c")));
        assert!(path_matches(candidate, Path::new("src/main.c")));
        assert!(path_matches(candidate, Path::new("./main.c")));
        assert!(path_matches(candidate, Path::new("src/./main.c")));
        assert!(!path_matches(candidate, Path::new(".")));
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
                symbol_sources: crate::model::SymbolTableSources::default(),
                globals: Vec::new(),
                types: Arc::default(),
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
                sections: Vec::new(),
            },
        )
    }

    #[test]
    fn instruction_starts_come_from_out_of_line_ranges_code_symbols_and_code_sections() {
        use crate::BoundaryEvidence::{CodeSymbol, FunctionRange, SectionStart};

        let code_symbol = |id, name: &str, start, end| SymbolInfo {
            id: SymbolId::new(id),
            name: name.into(),
            address: ImageAddress::new(start),
            kind: SymbolKind::Function,
            binding: SymbolBinding::Global,
            exported: true,
            extent: Some(SymbolExtent {
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                provenance: SymbolExtentProvenance::Declared,
            }),
            storage: None,
        };
        let section = |id, name: &str, start, end, executable| SectionInfo {
            id: SectionId::new(id),
            name: name.into(),
            range: AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            },
            executable,
            writable: !executable,
        };
        let image = ModuleImage::new(
            PathBuf::from("/test/starts"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(0x100),
            },
            ModuleMetadata {
                functions: vec![FunctionInfo {
                    id: FunctionId::new(0),
                    name: "split".into(),
                    linkage_name: None,
                    declaration: None,
                }],
                code_instances: vec![
                    // A function split into two ranges, and an inline
                    // expansion whose start proves nothing on its own.
                    instance(
                        0,
                        0,
                        None,
                        CodeInstanceKind::OutOfLine,
                        &[(0x20, 0x30), (0x60, 0x68)],
                    ),
                    instance(
                        1,
                        0,
                        Some(0),
                        CodeInstanceKind::Inline { call_site: None },
                        &[(0x24, 0x28)],
                    ),
                ],
                // A symbol at a function's start is weaker evidence than
                // the function itself.
                symbols: vec![
                    code_symbol(0, "split", 0x20, 0x30),
                    code_symbol(1, "symbol_only", 0x40, 0x48),
                ],
                symbol_sources: SymbolTableSources::default(),
                globals: Vec::new(),
                types: Arc::default(),
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
                sections: vec![
                    section(0, ".text", 0x10, 0x70, true),
                    section(1, ".data", 0x80, 0x90, false),
                ],
            },
        );
        let starts = |start, end| {
            image
                .instruction_starts(AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                })
                .map(|(address, evidence)| (address.get(), evidence))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            starts(0, 0x100),
            [
                (0x10, SectionStart),
                (0x20, FunctionRange),
                (0x40, CodeSymbol),
                (0x60, FunctionRange),
            ]
        );
        assert_eq!(
            starts(0x20, 0x60),
            [(0x20, FunctionRange), (0x40, CodeSymbol)]
        );
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
                symbol_sources: crate::model::SymbolTableSources::default(),
                globals: Vec::new(),
                types: Arc::default(),
                source_files: Vec::new(),
                statements: boundary_test_rows(),
                lines: Vec::new(),
                sections: Vec::new(),
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

    /// One test symbol: name, start, end (equal for a symbol without an
    /// extent), provenance, kind, binding, and whether it is exported.
    type TestSymbol = (
        &'static str,
        u64,
        u64,
        SymbolExtentProvenance,
        SymbolKind,
        SymbolBinding,
        bool,
    );

    fn symbol_test_image(symbols: &[TestSymbol]) -> ModuleImage {
        sectioned_symbol_test_image(symbols, &[])
    }

    /// Builds an image of symbols and sections, each section given by name,
    /// start, end, and whether it is executable.
    fn sectioned_symbol_test_image(
        symbols: &[TestSymbol],
        sections: &[(&'static str, u64, u64, bool)],
    ) -> ModuleImage {
        let sections = sections
            .iter()
            .enumerate()
            .map(|(index, &(name, start, end, executable))| SectionInfo {
                id: SectionId::new(u32::try_from(index).expect("test section count")),
                name: name.into(),
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                executable,
                writable: !executable,
            })
            .collect();
        let symbols = symbols
            .iter()
            .enumerate()
            .map(
                |(index, &(name, start, end, provenance, kind, binding, exported))| SymbolInfo {
                    id: SymbolId::new(u32::try_from(index).expect("test symbol count")),
                    name: name.into(),
                    address: ImageAddress::new(start),
                    kind,
                    binding,
                    exported,
                    extent: (start < end && kind != SymbolKind::Data).then_some(SymbolExtent {
                        range: AddressRange {
                            start: ImageAddress::new(start),
                            end: ImageAddress::new(end),
                        },
                        provenance,
                    }),
                    storage: (kind == SymbolKind::Data).then_some(AddressRange {
                        start: ImageAddress::new(start),
                        end: ImageAddress::new(end),
                    }),
                },
            )
            .collect();
        ModuleImage::new(
            PathBuf::from("/test/symbols"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(0x1000),
            },
            ModuleMetadata {
                functions: Vec::new(),
                code_instances: Vec::new(),
                symbols,
                symbol_sources: SymbolTableSources::default(),
                globals: Vec::new(),
                types: Arc::default(),
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
                sections,
            },
        )
    }

    fn symbolized(image: &ModuleImage, address: u64) -> Option<(&str, u64)> {
        image.symbolize(ImageAddress::new(address)).map(|location| {
            (
                image.symbol(location.symbol).expect("known").name.as_ref(),
                location.offset,
            )
        })
    }

    #[test]
    fn symbolization_uses_only_containing_extents_and_prefers_declared_innermost_code() {
        use SymbolBinding::{Global, Local};
        use SymbolExtentProvenance::{Declared, Inferred};
        use SymbolKind::{Data, Function};

        let image = symbol_test_image(&[
            ("outer", 0x100, 0x200, Declared, Function, Global, true),
            ("inner", 0x140, 0x160, Declared, Function, Local, false),
            // An unsized label inside a sized function never displaces it.
            ("label", 0x180, 0x190, Inferred, Function, Global, true),
            ("tail", 0x300, 0x340, Inferred, Function, Local, false),
            // Symbols without an extent never name code.
            ("object", 0x240, 0x240, Declared, Data, Global, true),
        ]);

        assert_eq!(symbolized(&image, 0xff), None);
        assert_eq!(symbolized(&image, 0x100), Some(("outer", 0)));
        assert_eq!(symbolized(&image, 0x13f), Some(("outer", 0x3f)));
        assert_eq!(symbolized(&image, 0x140), Some(("inner", 0)));
        assert_eq!(symbolized(&image, 0x15f), Some(("inner", 0x1f)));
        assert_eq!(symbolized(&image, 0x160), Some(("outer", 0x60)));
        assert_eq!(symbolized(&image, 0x185), Some(("outer", 0x85)));
        assert_eq!(symbolized(&image, 0x1ff), Some(("outer", 0xff)));
        // No nearest preceding symbol is guessed for unnamed code.
        assert_eq!(symbolized(&image, 0x200), None);
        assert_eq!(symbolized(&image, 0x240), None);
        assert_eq!(symbolized(&image, 0x33f), Some(("tail", 0x3f)));
        assert_eq!(
            image
                .symbolize(ImageAddress::new(0x300))
                .map(|location| location.provenance),
            Some(Inferred)
        );
        assert_eq!(symbolized(&image, 0x340), None);
    }

    #[test]
    fn same_extent_aliases_resolve_by_kind_binding_export_and_spelling() {
        use SymbolBinding::{Global, Local, Weak};
        use SymbolExtentProvenance::Declared;
        use SymbolKind::{Function, IndirectFunction};

        // Each row adds a candidate that outranks every earlier one by
        // exactly one rule and sorts after them by name, so only that rule
        // can select it.
        let ladder: [TestSymbol; 6] = [
            (
                "_a_resolver",
                0x10,
                0x20,
                Declared,
                IndirectFunction,
                Global,
                true,
            ),
            ("_b_local", 0x10, 0x20, Declared, Function, Local, false),
            ("_c_weak", 0x10, 0x20, Declared, Function, Weak, false),
            ("_d_hidden", 0x10, 0x20, Declared, Function, Global, false),
            ("_e_exported", 0x10, 0x20, Declared, Function, Global, true),
            ("f_exported", 0x10, 0x20, Declared, Function, Global, true),
        ];
        for count in 1..=ladder.len() {
            let image = symbol_test_image(&ladder[..count]);
            assert_eq!(
                symbolized(&image, 0x18).map(|(name, _)| name),
                Some(ladder[count - 1].0),
                "{count} candidates"
            );
        }

        // With every rule tied, the bytewise-smallest name wins regardless
        // of catalog order.
        let tied = symbol_test_image(&[
            ("beta", 0x10, 0x20, Declared, Function, Global, true),
            ("alpha", 0x10, 0x20, Declared, Function, Global, true),
        ]);
        assert_eq!(symbolized(&tied, 0x10), Some(("alpha", 0)));
    }

    #[test]
    fn descriptions_prefer_code_then_declared_storage_and_never_guess_a_neighbor() {
        use SymbolBinding::{Global, Local};
        use SymbolExtentProvenance::{Declared, Inferred};
        use SymbolKind::{Data, Function};

        let image = sectioned_symbol_test_image(
            &[
                ("function", 0x100, 0x140, Declared, Function, Global, true),
                // A data object inside code never displaces the function.
                ("table", 0x120, 0x130, Declared, Data, Global, true),
                ("outer", 0x800, 0x840, Declared, Data, Global, true),
                ("inner", 0x810, 0x818, Declared, Data, Local, false),
                // Unsized data names only its own address.
                ("label", 0x880, 0x880, Declared, Data, Global, true),
                ("inside", 0x820, 0x820, Declared, Data, Global, true),
            ],
            &[
                (".text", 0x100, 0x200, true),
                (".data", 0x800, 0x900, false),
                // A malformed overlapping section loses to the innermost one.
                (".overlap", 0x7f0, 0x8f0, false),
            ],
        );
        let described = |address: u64| {
            let description = image.describe(ImageAddress::new(address));
            (
                description
                    .section
                    .map(|section| (section.name.to_string(), section.offset)),
                description.symbol.map(|symbol| {
                    (
                        symbol.name.to_string(),
                        symbol.kind,
                        symbol.offset,
                        symbol.provenance,
                    )
                }),
            )
        };
        let text = |offset| Some((".text".to_owned(), offset));
        let data = |offset| Some((".data".to_owned(), offset));
        let symbol = |name: &str, kind, offset, provenance| {
            Some((name.to_owned(), kind, offset, provenance))
        };

        assert_eq!(
            described(0x124),
            (text(0x24), symbol("function", Function, 0x24, Declared))
        );
        assert_eq!(described(0x150), (text(0x50), None));
        assert_eq!(
            described(0x814),
            (data(0x14), symbol("inner", Data, 4, Declared))
        );
        assert_eq!(
            described(0x818),
            (data(0x18), symbol("outer", Data, 0x18, Declared))
        );
        // Sized storage wins over an unsized label at the same address.
        assert_eq!(
            described(0x820),
            (data(0x20), symbol("outer", Data, 0x20, Declared))
        );
        assert_eq!(described(0x840), (data(0x40), None));
        assert_eq!(
            described(0x880),
            (data(0x80), symbol("label", Data, 0, Inferred))
        );
        assert_eq!(described(0x881), (data(0x81), None));
        assert_eq!(described(0x7f8), (Some((".overlap".to_owned(), 8)), None));
        assert_eq!(described(0x900), (None, None));
    }
}
