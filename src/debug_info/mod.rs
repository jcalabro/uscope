#[cfg(not(target_os = "linux"))]
compile_error!("uscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod dwarf;
#[cfg(target_os = "linux")]
mod elf;
mod roles;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

#[cfg(feature = "fuzzing")]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    dwarf::fuzz_expression(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_elf_symbols(data: &[u8]) {
    elf::fuzz(data);
}

use std::path::Path;
use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{
    CodeInstanceId, DereferenceReference, DereferencedValue, GlobalVariableId, ImageAddress,
    InspectedValue, ModuleId, ModuleImage, ModuleImageId, RegisterDescriptor, Result, StackFrameId,
    StopId, ThreadId, TypeId, UnwindTermination, ValueChildPage, ValueChildrenReference, Variable,
    VariableQuery, VariableState, VariableUnavailableReason, VirtualAddress,
};

#[derive(Debug, Clone, Copy)]
pub struct VariableContext {
    pub stop_id: StopId,
    pub thread: ThreadId,
    /// The backtrace frame whose registers and call-frame address evaluate
    /// the values, which every capability they produce keeps.
    pub frame: StackFrameId,
    pub module: ModuleId,
    pub image: ModuleImageId,
    pub address: Option<ImageAddress>,
}

pub struct VariableRegister {
    pub descriptor: RegisterDescriptor,
    pub bytes: Arc<[u8]>,
}

/// Where a data object's storage lives, which bounds how long its address
/// identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    /// The value is not held in addressable memory (a constant, a register,
    /// a computed value, or no location at all).
    NotMemory,
    /// Static storage at a module-relative address.
    Static,
    /// One thread's instance of thread-local storage.
    ThreadLocal,
    /// Storage addressed relative to a function activation.
    Frame {
        /// One location expression applies throughout the object's scope, so
        /// its address cannot move while the activation lives.
        stable: bool,
        /// The language runtime may move the stack holding the object.
        moving_stack: bool,
    },
    /// The address is computed from memory read during evaluation.
    Indirect,
}

/// The storage of one data object and the instructions where it is in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStorage {
    pub class: StorageClass,
    pub ranges: Arc<[crate::AddressRange<ImageAddress>]>,
}

/// One data object an image catalogs: a local, a parameter, or a global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectKey(usize);

/// A typed value's storage in one image, found at one stop.
#[derive(Debug, Clone)]
pub struct Located {
    pub ty: TypeId,
    pub storage: ValueStorage,
}

/// Storage reached, or the unavailable or malformed state that stopped it.
pub type Accessed = std::result::Result<Located, VariableState>;

/// One structural step from a value to another.
#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    /// Through a pointer or reference.
    Deref,
    /// To a member, through any pointers to the record holding it.
    Member(&'a str),
    /// To an element of an array or slice, holding `available` index values,
    /// of which an array takes one per dimension and a slice one.
    Index { available: usize },
    /// To the one base class subobject of a record whose type is a target.
    Base(BaseTarget<'a>),
}

/// Which types a [`Step::Base`] reaches: those its caller deems the same
/// type, as one type defined in several units is.
#[derive(Clone, Copy)]
pub struct BaseTarget<'a> {
    /// The base's name, for errors.
    pub name: &'a str,
    pub is_target: &'a dyn Fn(TypeId) -> bool,
}

impl std::fmt::Debug for BaseTarget<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BaseTarget")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A step planned from types alone, which [`VariableInfo::apply`] follows at
/// a stop.
#[derive(Clone)]
pub struct PlannedStep {
    steps: Vec<dwarf::PathStep>,
    /// How many index values the step takes.
    consumed: usize,
    /// The type the step reaches, unless planning found it unavailable.
    result: Option<TypeId>,
}

impl PlannedStep {
    pub const fn consumed(&self) -> usize {
        self.consumed
    }

    pub const fn result(&self) -> Option<TypeId> {
        self.result
    }

    /// Checks index values against an array's static bounds, which needs no
    /// program state, so that a bad index is an error before any storage is
    /// read.
    pub fn check_indices(&self, indices: &[i128]) -> Result<()> {
        self.steps
            .iter()
            .try_for_each(|step| dwarf::array_byte_offset(step, indices).map(drop))
    }
}

/// Where a string's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextLocation {
    /// The first byte's address.
    pub address: VirtualAddress,
    /// How many bytes there are, when the string records it rather than
    /// ending at a NUL.
    pub length: Option<u64>,
}

pub struct DebugInfo {
    pub image: Arc<ModuleImage>,
    pub unwind: Arc<dyn UnwindInfo>,
    pub variables: Arc<dyn VariableInfo>,
}

#[derive(Clone)]
pub enum VariableRuntimeError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
    Fatal(Arc<str>),
}

impl From<VariableUnavailableReason> for VariableRuntimeError {
    fn from(reason: VariableUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

pub trait VariableRuntime {
    fn register(
        &mut self,
        register: u16,
    ) -> std::result::Result<VariableRegister, VariableRuntimeError>;
    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError>;
    fn tls_address(
        &mut self,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, VariableUnavailableReason>;
    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>>;
    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError>;
}

pub trait VariableInfo: Send + Sync {
    /// Inspects the parameters and local variables visible in one selected logical
    /// frame. `selected` names the presented inline instance, or `None` for
    /// the physical frame; data objects belonging to other logical frames at the
    /// same address are out of scope.
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>>;

    /// The local or parameter `name` names in the selected logical frame,
    /// whose innermost declaration hides the others.
    fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<ObjectKey>;

    /// The data object of one cataloged global.
    fn global_object(&self, id: GlobalVariableId) -> Result<ObjectKey>;

    /// The object's type, or why its type is malformed.
    fn object_type(&self, object: ObjectKey) -> std::result::Result<TypeId, Arc<str>>;

    /// Plans one step from a value of type `from`, reading no program state.
    fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep>;

    /// Finds one data object's storage at `address`, the frame's
    /// module-relative instruction, if any.
    fn locate(
        &self,
        object: ObjectKey,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed>;

    /// Follows a planned step from `from`, with `indices` as its index
    /// values. An index outside an array's static bounds is an error.
    fn apply(
        &self,
        from: &Located,
        step: &PlannedStep,
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed>;

    /// Decodes the scalar stored at `at`, without the text it may point to.
    fn load(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<crate::VariableValue, VariableState>>;

    /// Where the bytes of the text stored at `at` are, when it is a string:
    /// the first byte's address, and how many there are when the string
    /// records it rather than ending at a NUL.
    fn text_span(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<Option<TextLocation>, VariableState>> {
        let _ = (at, context, runtime, budget);
        Ok(Ok(None))
    }

    /// Decodes the value stored at `at`, with its children and dereference
    /// capabilities.
    fn materialize(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue>;

    /// Classifies where one data object's storage lives.
    fn object_storage(&self, object: ObjectKey) -> ObjectStorage;

    /// Evaluates one cataloged global at the selected thread's current stop.
    /// `address` is the module-relative program counter, `None` outside this
    /// module, where a range-gated location is unavailable.
    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable>;

    /// Dereferences one stop-scoped capability produced by this image.
    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue>;

    /// Evaluates one arbitrary bounded interval from a stop-scoped aggregate
    /// capability.
    fn value_children(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage>;
}

/// Call-frame information for one module image. Instruction addresses are
/// image addresses; registers and memory belong to the stopped process.
pub trait UnwindInfo: Send + Sync {
    /// Computes the canonical frame address of the frame executing `address`.
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination>;

    /// Reconstructs the caller of the frame executing `address`.
    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination>;
}

pub fn load_bytes(path: &Path, data: &[u8]) -> Result<DebugInfo> {
    dwarf::load_bytes(path, data, crate::ModuleImageId::new(0))
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_module(path: &Path, id: crate::ModuleImageId) -> Result<DebugInfo> {
    dwarf::load(path, id)
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_module_bytes(
    path: &Path,
    data: &[u8],
    id: crate::ModuleImageId,
) -> Result<DebugInfo> {
    dwarf::load_bytes(path, data, id)
}
