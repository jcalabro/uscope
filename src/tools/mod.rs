//! Developer tools behind `uscope-tools`: a canonical dump of every answer
//! a program's debug information gives, and one measured load for
//! benchmarks.

pub mod dump;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::profile::{Options, Recording, Report};

/// One load of one program, measured in a process of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measured {
    /// Whether the load succeeded, or why it failed.
    pub outcome: Result<(), String>,
    /// What the load cost.
    pub report: Report,
    /// The heap the loaded debug information holds, net of what the load
    /// freed.
    pub retained_heap_bytes: Option<i64>,
    /// What freeing each part of the loaded debug information returns, in
    /// order: the variable provider, the unwinder, then the image and what
    /// they shared with it.
    #[serde(default)]
    pub retained_parts: Vec<(String, i64)>,
}

/// Loads `path` as the debugger loads a module, recording what it costs.
pub fn measure_load(path: &Path, instructions: bool) -> anyhow::Result<Measured> {
    let recording = Recording::start(Options { instructions })?;
    let before = crate::profile::alloc::totals();
    let loaded = crate::debug_info::load_module(
        path,
        crate::ModuleImageId::new(0),
        &crate::debug_info::DebugFileSearch::default(),
    );
    let retained = crate::profile::alloc::totals().since(&before).live_bytes;
    let report = recording.finish();
    let counted = report.summary.allocations.is_some();
    let mut retained_parts = Vec::new();
    let outcome = loaded.map(|loaded| {
        let live = || crate::profile::alloc::totals().live_bytes;
        let mut freed = |name: &str, part: Box<dyn FnOnce() + '_>| {
            let held = live();
            part();
            retained_parts.push((name.to_owned(), held - live()));
        };
        let crate::debug_info::DebugInfo {
            image,
            unwind,
            variables,
        } = loaded;
        freed("variables", Box::new(move || drop(variables)));
        freed("unwind", Box::new(move || drop(unwind)));
        freed("image", Box::new(move || drop(image)));
    });
    Ok(Measured {
        outcome: outcome.map_err(|error| error.to_string()),
        report,
        retained_heap_bytes: counted.then_some(retained),
        retained_parts: if counted { retained_parts } else { Vec::new() },
    })
}
