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

/// The code of a source, without its comments and the text of its string
/// literals, which may name what the code may not use.
fn code(source: &str) -> String {
    let mut code = String::with_capacity(source.len());
    for line in source.lines() {
        let line = line.split("//").next().unwrap_or_default();
        let mut quoted = false;
        let mut escaped = false;
        for character in line.chars() {
            match (quoted, escaped, character) {
                (true, false, '\\') => escaped = true,
                (true, true, _) => escaped = false,
                (_, false, '"') => {
                    quoted = !quoted;
                    code.push('"');
                }
                (true, false, _) => {}
                (false, _, character) => code.push(character),
            }
        }
        code.push('\n');
    }
    code
}

/// A runtime model is pure: it reaches a program only through the traits
/// the debugger implements for it, so none of its code may reach for
/// process control, debug-information parsing, I/O, clocks, or threads.
/// A model may name a runtime's types and functions in its strings, but
/// never use the runtime's crate.
#[test]
fn runtime_model_stays_pure() {
    const FORBIDDEN: [&str; 13] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "gimli",
        "object::",
        "use tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
        "libc",
    ];
    let sources = sources(&source_root().join("runtime_model"));
    for (path, source) in &sources {
        let code = code(source);
        for forbidden in FORBIDDEN {
            assert!(
                !code.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
        // The tokio model's own module is named for the runtime it reads.
        assert!(
            code.match_indices("tokio::")
                .all(|(at, _)| code[..at].ends_with("self::") || code[..at].ends_with("super::")),
            "{} uses `tokio::`",
            path.display()
        );
    }
    assert!(sources.len() >= 3, "the runtime models' sources were found");
    // The check sees through neither comments nor strings.
    assert!(code("let x = tokio::spawn(f);").contains("tokio::spawn"));
    assert!(!code("let x = \"tokio::spawn\"; // tokio::spawn").contains("tokio::"));
}

/// Each runtime is named in its own model only. Run control, unwinding,
/// the protocol, and the clients speak of tasks and code roles, and learn
/// what a runtime is through its model, never by recognizing its names.
#[test]
fn languages_stay_at_their_seams() {
    // Each row names a runtime in the way only its model, and the
    // debug-information provider's code roles, should.
    const RUNTIMES: [(&str, &[&str]); 3] = [
        ("runtime_model/go", &["\"runtime.", "allgs", "goroutine", "goid"]),
        (
            "runtime_model/tokio",
            &["ownedtasks", "\"tokio::runtime", "current_task_id", "context::context"],
        ),
        ("runtime_model/rust", &["rust_panic", "rust_begin_unwind"]),
    ];
    let root = source_root();
    for directory in [
        "backend",
        "cli",
        "dap",
        "eval",
        "web",
        "present",
        "runtime_model",
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
            // Documentation may name a runtime as an example; code may not.
            let code = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            for (home, names) in RUNTIMES {
                if path.starts_with(root.join(home)) {
                    continue;
                }
                for name in names {
                    assert!(
                        !code.contains(name),
                        "{} names a runtime with `{name}`; ask its model instead",
                        path.display()
                    );
                }
            }
        }
    }
}
