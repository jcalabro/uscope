//! Embeds the web UI's built files in release builds of uscope.
//!
//! Development builds read `build/web` at run time instead, so rebuilding
//! the page never rebuilds the crate. Node is never needed to build uscope:
//! a missing `build/web` embeds nothing, and the page says to run
//! `just web`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    let root = manifest.join("build/web");
    let mut files = Vec::new();
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo::rerun-if-changed={}", root.display());
        collect(&root, &root, &mut files);
        files.sort();
    }
    let mut table = String::from("&[\n");
    for (name, path) in files {
        writeln!(
            table,
            "    ({name:?}, include_bytes!({path:?}).as_slice()),"
        )
        .expect("writing to a string");
    }
    table.push(']');
    std::fs::write(out.join("web_assets.rs"), table).expect("write the asset table");
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push((
                relative.to_string_lossy().into_owned(),
                path.to_string_lossy().into_owned(),
            ));
        }
    }
}
