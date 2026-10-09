//! Embeds the web UI's built files in release builds of uscope, and names
//! the sources the loader was built from.
//!
//! Development builds read `build/web` at run time instead, so rebuilding
//! the page never rebuilds the crate. Node is never needed to build uscope:
//! a missing `build/web` embeds nothing, and the page says to run
//! `just web`.

use std::fmt::Write as _;
use std::hash::Hasher as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    sources_digest(&manifest);
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

/// Digests every source file and the locked dependencies, so that images a
/// cache holds are never read by a loader built from other sources: any
/// change may change what a load produces.
fn sources_digest(manifest: &Path) {
    let mut files = Vec::new();
    for root in ["src", "Cargo.lock"] {
        println!("cargo::rerun-if-changed={root}");
        let path = manifest.join(root);
        if path.is_dir() {
            collect(manifest, &path, &mut files);
        } else {
            files.push((root.to_owned(), path.to_string_lossy().into_owned()));
        }
    }
    files.sort();
    // SipHash with fixed keys: stable for one toolchain, and a toolchain
    // that changes it only empties caches.
    let mut digest = std::hash::DefaultHasher::new();
    for (name, path) in files {
        let contents = std::fs::read(&path).expect("read a source file");
        digest.write(name.as_bytes());
        digest.write_usize(contents.len());
        digest.write(&contents);
    }
    println!(
        "cargo::rustc-env=USCOPE_SOURCES_DIGEST={:016x}",
        digest.finish()
    );
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
