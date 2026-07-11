#[cfg(not(target_os = "linux"))]
compile_error!("uscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod dwarf;

use std::path::Path;
use std::sync::Arc;

use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{ImageAddress, ModuleImage, Result, UnwindTermination};

pub struct DebugInfo {
    pub image: Arc<ModuleImage>,
    pub unwind: Arc<dyn UnwindInfo>,
}

pub trait UnwindInfo: Send + Sync {
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
