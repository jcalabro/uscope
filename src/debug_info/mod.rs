#[cfg(not(target_os = "linux"))]
compile_error!("uscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod dwarf;

use std::path::Path;
use std::sync::Arc;

use crate::{ModuleImage, Result};

pub fn load(path: &Path) -> Result<Arc<ModuleImage>> {
    dwarf::load(path)
}
