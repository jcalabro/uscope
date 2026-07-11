#[cfg(not(target_os = "linux"))]
compile_error!("uscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod dwarf;

use std::path::Path;
use std::sync::Arc;

use crate::Result;

pub trait DebugInfo: Send + Sync {
    fn function_address(&self, name: &str) -> Result<u64>;
    fn symbol_address(&self, name: &str) -> Result<u64>;
}

pub fn load(path: &Path) -> Result<Arc<dyn DebugInfo>> {
    dwarf::load(path)
}
