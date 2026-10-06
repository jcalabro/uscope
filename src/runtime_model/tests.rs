use std::path::{Path, PathBuf};

/// Every Rust source under `directory`, with the code compiled only for
/// tests removed.
fn sources(directory: &Path) -> Vec<(PathBuf, String)> {
    let mut pending = vec![directory.to_owned()];
    let mut sources = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            let entries = std::fs::read_dir(&path).expect("read a source directory");
            pending.extend(entries.map(|entry| entry.expect("a directory entry").path()));
            continue;
        }
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

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// A runtime model is pure: it reaches a program only through the traits
/// the debugger implements for it, so none of its code may reach for
/// process control, debug-information parsing, I/O, clocks, or threads.
#[test]
fn runtime_model_stays_pure() {
    const FORBIDDEN: [&str; 13] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "gimli",
        "object::",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
        "libc",
    ];
    let sources = sources(&source_root().join("runtime_model"));
    for (path, source) in &sources {
        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
    }
    assert!(sources.len() >= 3, "the runtime models' sources were found");
}

/// Go's runtime is named in its own model only. Run control, unwinding,
/// the protocol, and the clients speak of tasks and code roles, and learn
/// what Go is through the model, never by recognizing its names.
#[test]
fn languages_stay_at_their_seams() {
    // Each names the runtime of Go in the way a model should.
    const RUNTIME_NAMES: [&str; 4] = ["\"runtime.", "allgs", "goroutine", "goid"];
    let root = source_root();
    let allowed = [root.join("runtime_model/go"), root.join("debug_info")];
    // Each still knows Go's runtime, until its knowledge moves into the
    // model, and says why.
    let pending = [(
        root.join("backend/linux/presentation.rs"),
        "the interface convention, which reads `runtime.types`",
    )];
    for directory in [
        "backend",
        "cli",
        "dap",
        "eval",
        "unwind.rs",
        "protocol.rs",
        "lib.rs",
    ] {
        let path = root.join(directory);
        let found = if path.is_dir() {
            sources(&path)
        } else {
            let source = std::fs::read_to_string(&path).expect("read a source file");
            let source = source
                .split("#[cfg(test)]")
                .next()
                .unwrap_or_default()
                .to_owned();
            vec![(path, source)]
        };
        for (path, source) in found {
            if allowed.iter().any(|allowed| path.starts_with(allowed))
                || pending.iter().any(|(pending, _)| *pending == path)
            {
                continue;
            }
            // Documentation may name a runtime as an example; code may not.
            let code = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            for name in RUNTIME_NAMES {
                assert!(
                    !code.contains(name),
                    "{} names Go's runtime with `{name}`; ask a runtime model instead",
                    path.display()
                );
            }
        }
    }
}
