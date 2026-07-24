#[cfg(not(target_os = "linux"))]
compile_error!("uscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod dwarf;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

#[cfg(feature = "fuzzing")]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    dwarf::fuzz_expression(data);
}

use std::path::Path;
use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{
    CodeInstanceId, DereferenceReference, DereferencedValue, GlobalVariableId, ImageAddress,
    InspectedValue, ModuleId, ModuleImage, ModuleImageId, RegisterDescriptor, Result, StopId,
    ThreadId, UnwindTermination, ValueChildPage, ValueChildrenReference, ValuePathStep, Variable,
    VariableQuery, VariableUnavailableReason, VirtualAddress,
};

#[derive(Debug, Clone, Copy)]
pub struct VariableContext {
    pub stop_id: StopId,
    pub thread: ThreadId,
    pub module: ModuleId,
    pub image: ModuleImageId,
    pub address: Option<ImageAddress>,
}

pub struct VariableRegister {
    pub descriptor: RegisterDescriptor,
    pub bytes: Arc<[u8]>,
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

    /// Inspects one visible local or parameter and follows a structural member
    /// path atomically within one stopped-state validation.
    #[expect(
        clippy::too_many_arguments,
        reason = "the provider boundary keeps frame identity, path, runtime, and budget explicit"
    )]
    fn inspect_path(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        root: &str,
        selectors: &[ValuePathStep],
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue>;

    /// Evaluates one cataloged global at the selected thread's current stop.
    ///
    /// `address` is the module-relative instruction context, or `None` when the
    /// stopped thread's program counter does not fall within this module. A
    /// range-gated location that cannot be selected without a context resolves
    /// to an explicit unavailable state rather than a guessed address.
    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable>;

    /// Evaluates one cataloged global and follows a structural member path
    /// atomically within one stopped-state validation.
    fn inspect_global_path(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        selectors: &[ValuePathStep],
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue>;

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

pub trait UnwindInfo: Send + Sync {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
    ) -> std::result::Result<VirtualAddress, UnwindTermination>;

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination>;
}

pub fn load(path: &Path) -> Result<DebugInfo> {
    dwarf::load(path, crate::ModuleImageId::new(0))
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_module(path: &Path, id: crate::ModuleImageId) -> Result<DebugInfo> {
    dwarf::load(path, id)
}
