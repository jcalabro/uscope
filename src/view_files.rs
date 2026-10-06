//! The view files a client loads for a session, besides those it is
//! given.
//!
//! They are the project's, every `*.views` file in `.uscope/views` under the
//! working directory, and the user's, in `$XDG_CONFIG_HOME/uscope/views` or
//! `~/.config/uscope/views`, each directory's in name order. A project's
//! views come before the user's, being about the project's own types.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::view::syntax::MAX_FILE_BYTES;

/// One view file's name, as errors and `info view` give it, and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFile {
    pub name: String,
    pub text: String,
}

/// The project's and then the user's view files, and why any could not be
/// read. A directory that does not exist holds none.
#[must_use]
pub fn discover(working_directory: &Path) -> (Vec<ViewFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let directories = [
        Some(working_directory.join(".uscope/views")),
        user_directory(),
    ];
    for directory in directories.into_iter().flatten() {
        let (found, failed) = read_directory(&directory);
        files.extend(found);
        errors.extend(failed);
    }
    (files, errors)
}

/// Where the user's view files are.
#[must_use]
pub fn user_directory() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("uscope/views"))
}

/// Reads one view file, at most one byte more than a view file may hold,
/// so that the parser refuses one too long without reading all of it.
pub fn read(path: &Path) -> Result<ViewFile, String> {
    let name = path.display().to_string();
    let file = fs::File::open(path).map_err(|error| format!("{name}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{name}: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{name}: the file is not UTF-8"))?;
    Ok(ViewFile { name, text })
}

/// Every `*.views` file in a directory, in name order.
fn read_directory(directory: &Path) -> (Vec<ViewFile>, Vec<String>) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), Vec::new());
        }
        Err(error) => {
            return (
                Vec::new(),
                vec![format!("{}: {error}", directory.display())],
            );
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "views")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        match read(&path) {
            Ok(file) => files.push(file),
            Err(error) => errors.push(error),
        }
    }
    (files, errors)
}
