//! The golden programs, loaded once and shared read-only by every session.
//!
//! Each program in `tests/golden` has a manifest, written by
//! `scripts/golden.sh`, naming its compiled variants and what each run of
//! it prints and returns.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use object::{Object as _, ObjectSymbol as _};
use serde::Deserialize;

use super::loader::Image;
use crate::debug_info::{self, DebugInfo};

/// Where the golden programs live, in the source tree this build came from.
#[must_use]
pub fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Where a golden program's sources and binaries are in simulated paths,
/// which match the directory its debug information names.
const SIMULATED_ROOT: &str = "/uscope/tests/golden";

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("{path}: {error}")]
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("{path}: {error}")]
    Manifest {
        path: PathBuf,
        error: serde_json::Error,
    },
    #[error("{path}: {error}")]
    Load { path: PathBuf, error: String },
}

#[derive(Deserialize)]
struct Manifest {
    program: String,
    variants: Vec<ManifestVariant>,
    runs: Vec<Run>,
}

#[derive(Deserialize)]
struct ManifestVariant {
    name: String,
}

/// One way to run a program, and what it does when nothing interferes.
#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub arguments: Vec<String>,
    pub exit_code: i32,
    pub output: String,
}

/// One compiled form of a program.
pub struct Variant {
    /// The binary's file name, such as `straight-gcc-O0`.
    pub name: String,
    /// The binary in the source tree, which tests also run natively.
    pub file: PathBuf,
    /// The binary's path inside the simulation.
    pub path: Arc<str>,
    /// The inode simulated mappings of the binary report.
    pub inode: u64,
    pub data: Arc<[u8]>,
    pub image: Arc<Image>,
    /// The functions the binary defines, by name.
    pub functions: Vec<String>,
    debug_info: DebugInfo,
}

impl Variant {
    /// The debug information a session's controller takes, sharing what
    /// was loaded.
    #[must_use]
    pub fn debug_info(&self) -> DebugInfo {
        DebugInfo {
            image: Arc::clone(&self.debug_info.image),
            unwind: Arc::clone(&self.debug_info.unwind),
            variables: Arc::clone(&self.debug_info.variables),
        }
    }
}

pub struct Program {
    pub name: String,
    pub variants: Vec<Variant>,
    pub runs: Vec<Run>,
    /// The functions any variant defines, by name. Another variant may
    /// have inlined some of them away.
    pub functions: Vec<String>,
    /// The program's source file, as its debug information names it.
    pub source: PathBuf,
    /// How many lines the source file has.
    pub source_lines: u64,
}

pub struct Corpus {
    pub programs: Vec<Program>,
}

impl Corpus {
    /// Loads every program under [`directory`], in name order.
    pub fn load() -> Result<Self, CorpusError> {
        let root = directory();
        let io = |path: &Path| {
            let path = path.to_owned();
            move |error| CorpusError::Io { path, error }
        };
        let mut manifests = std::fs::read_dir(&root)
            .map_err(io(&root))?
            .filter_map(|entry| Some(entry.ok()?.path().join("manifest.json")))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        manifests.sort();

        let mut programs = Vec::new();
        let mut next_inode = 1;
        for path in manifests {
            let text = std::fs::read_to_string(&path).map_err(io(&path))?;
            let manifest: Manifest =
                serde_json::from_str(&text).map_err(|error| CorpusError::Manifest {
                    path: path.clone(),
                    error,
                })?;
            let mut variants = Vec::new();
            for entry in &manifest.variants {
                let name = format!("{}-{}", manifest.program, entry.name);
                let file = root.join(&manifest.program).join(&name);
                let data: Arc<[u8]> = std::fs::read(&file).map_err(io(&file))?.into();
                let simulated = format!("{SIMULATED_ROOT}/{}/{name}", manifest.program);
                let load = |error: String| CorpusError::Load {
                    path: file.clone(),
                    error,
                };
                let image = Image::new(&simulated, next_inode, &data)
                    .map_err(|error| load(error.to_string()))?;
                let debug_info = debug_info::load_bytes(Path::new(&simulated), &data)
                    .map_err(|error| load(error.to_string()))?;
                let functions = defined_functions(&data).map_err(load)?;
                variants.push(Variant {
                    name,
                    file,
                    path: Arc::from(simulated),
                    inode: next_inode,
                    data,
                    image: Arc::new(image),
                    functions,
                    debug_info,
                });
                next_inode += 1;
            }
            let functions = variants
                .iter()
                .flat_map(|variant| variant.functions.iter().cloned())
                .collect::<std::collections::BTreeSet<_>>();
            let source_file = root
                .join(&manifest.program)
                .join(format!("{}.c", manifest.program));
            let source_lines = std::fs::read_to_string(&source_file)
                .map_err(io(&source_file))?
                .lines()
                .count() as u64;
            programs.push(Program {
                source: PathBuf::from(format!("{SIMULATED_ROOT}/{0}/{0}.c", manifest.program)),
                source_lines,
                functions: functions.into_iter().collect(),
                name: manifest.program,
                variants,
                runs: manifest.runs,
            });
        }
        Ok(Self { programs })
    }
}

/// The functions a binary's symbol table defines, other than the
/// runtime's entry points.
fn defined_functions(data: &[u8]) -> Result<Vec<String>, String> {
    let file = object::File::parse(data).map_err(|error| error.to_string())?;
    let mut functions = file
        .symbols()
        .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
        .filter_map(|symbol| symbol.name().ok())
        .filter(|name| !name.starts_with('_'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    functions.sort();
    functions.dedup();
    Ok(functions)
}
