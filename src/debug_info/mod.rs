#[cfg(not(target_os = "linux"))]
compile_error!("uscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod dwarf;

use std::path::Path;
use std::sync::Arc;

use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{
    CodeInstanceId, ImageAddress, ModuleImage, Result, UnwindTermination, Variable, VariableQuery,
    VariableUnavailableReason, VirtualAddress,
};

pub struct DebugInfo {
    pub image: Arc<ModuleImage>,
    pub unwind: Arc<dyn UnwindInfo>,
    pub variables: Arc<dyn VariableInfo>,
}

pub trait VariableRuntime {
    fn register(&self, register: u16) -> Option<u64>;
    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableUnavailableReason>;
    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>>;
    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, Arc<str>>;
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
        runtime: &mut dyn VariableRuntime,
    ) -> Result<Vec<Variable>>;
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
    dwarf::load(path)
}
