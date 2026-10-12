use std::path::{Path, PathBuf};

/// Every Rust source under `directory`, with the code compiled only for
/// tests removed.
fn sources(directory: &Path) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(directory).expect("read the sources") {
        let path = entry.expect("a directory entry").path();
        let test_file = path.file_name().is_some_and(|name| name == "tests.rs");
        if test_file || path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read a source file");
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();
        sources.push((path, source.to_owned()));
    }
    sources.sort();
    sources
}

/// The model is pure: engines translate what they see into it at the edge,
/// so none of it may reach for process control, I/O, clocks, or threads,
/// and both engines and every client share it.
#[test]
fn the_model_stays_pure() {
    const FORBIDDEN: [&str; 12] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "libc",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
        "aya",
    ];
    let sources = sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/syscalls"));
    for (path, source) in &sources {
        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
    }
    assert!(sources.len() >= 8, "the model's sources were found");
}
