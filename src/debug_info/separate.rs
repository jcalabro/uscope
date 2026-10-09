//! Separate debug files: the debug information a module's own file leaves
//! out, as distributions ship it, found as gdb finds it.
//!
//! A module whose file has no DWARF names its debug file by build-id and by
//! a `.gnu_debuglink` file name and checksum. The build-id is looked up under
//! each debug directory's `.build-id`; the link beside the module, in its
//! `.debug` directory, and under each debug directory at the module's own
//! directory. A debuginfod server is asked last, and what it sends is kept in
//! debuginfod's cache, which gdb and other clients share. Every candidate
//! must prove it describes the module, by build-id or checksum. Only a
//! session that enables debuginfod asks a server.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use object::{Object, ObjectSection};

use crate::DebugFileOptions;

/// The debug directory every system has.
const SYSTEM_DEBUG_DIRECTORY: &str = "/usr/lib/debug";
/// How long a debuginfod download may take unless `DEBUGINFOD_TIMEOUT`
/// says otherwise, as debuginfod's own client allows.
const DEFAULT_DEBUGINFOD_TIMEOUT: Duration = Duration::from_secs(90);
/// The largest debug file a server may send.
const DOWNLOAD_LIMIT: u64 = 4 << 30;

/// Where one session looks for separate debug files.
#[derive(Debug, Clone, Default)]
pub struct DebugFileSearch {
    directories: Vec<PathBuf>,
    servers: Vec<String>,
    cache: Option<PathBuf>,
    timeout: Option<Duration>,
}

/// A separate debug file and its contents.
pub struct DebugFile {
    pub path: PathBuf,
    pub data: Vec<u8>,
}

impl DebugFileSearch {
    /// The search the options ask for, with the system's debug directories
    /// after theirs and debuginfod's settings from the environment where
    /// the options leave them out.
    pub fn new(options: &DebugFileOptions) -> Self {
        let mut directories = options.directories.clone();
        if let Some(listed) = std::env::var_os("NIX_DEBUG_INFO_DIRS") {
            directories.extend(std::env::split_paths(&listed));
        }
        directories.push(PathBuf::from(SYSTEM_DEBUG_DIRECTORY));
        let mut seen = std::collections::BTreeSet::new();
        directories.retain(|directory| {
            !directory.as_os_str().is_empty() && seen.insert(directory.clone())
        });
        let servers = if options.debuginfod {
            options.debuginfod_urls.clone().unwrap_or_else(|| {
                std::env::var("DEBUGINFOD_URLS")
                    .map(|urls| urls.split_whitespace().map(str::to_owned).collect())
                    .unwrap_or_default()
            })
        } else {
            Vec::new()
        };
        let timeout = std::env::var("DEBUGINFOD_TIMEOUT")
            .ok()
            .and_then(|seconds| seconds.trim().parse().ok())
            .map_or(DEFAULT_DEBUGINFOD_TIMEOUT, Duration::from_secs);
        Self {
            directories,
            servers,
            cache: options.debuginfod_cache.clone().or_else(default_cache),
            timeout: Some(timeout),
        }
    }

    /// The separate debug file of a module whose own file has no DWARF.
    pub fn find(&self, path: &Path, object: &object::File<'_>) -> Option<DebugFile> {
        if has_dwarf(object) {
            return None;
        }
        let build_id = object.build_id().ok().flatten().filter(|id| id.len() > 1);
        let describes = |data: &[u8]| debug_file_of(object, data, build_id, None);
        if let Some(id) = build_id {
            let hex = hex(id);
            for directory in &self.directories {
                let candidate = directory
                    .join(".build-id")
                    .join(&hex[..2])
                    .join(format!("{}.debug", &hex[2..]));
                if let Some(file) = read_if(&candidate, describes) {
                    return Some(file);
                }
            }
        }
        if let Ok(Some((name, checksum))) = object.gnu_debuglink() {
            let name = Path::new(OsStr::from_bytes(name));
            // A link names a file, never a path elsewhere.
            if name.file_name() == Some(name.as_os_str())
                && let Some(directory) = path.parent()
            {
                let mut candidates =
                    vec![directory.join(name), directory.join(".debug").join(name)];
                let relative = directory.strip_prefix("/").unwrap_or(directory);
                candidates.extend(
                    self.directories
                        .iter()
                        .map(|root| root.join(relative).join(name)),
                );
                for candidate in candidates {
                    if candidate == path {
                        continue;
                    }
                    let linked = |data: &[u8]| debug_file_of(object, data, None, Some(checksum));
                    if let Some(file) = read_if(&candidate, linked) {
                        return Some(file);
                    }
                }
            }
        }
        build_id.and_then(|id| self.download(object, id))
    }

    /// Asks the debuginfod servers for a build-id's debug file, which is
    /// first looked for in the cache, and kept there once downloaded.
    fn download(&self, object: &object::File<'_>, build_id: &[u8]) -> Option<DebugFile> {
        if self.servers.is_empty() {
            return None;
        }
        let hex = hex(build_id);
        let describes = |data: &[u8]| debug_file_of(object, data, Some(build_id), None);
        let cached = self
            .cache
            .as_ref()
            .map(|cache| cache.join(&hex).join("debuginfo"));
        if let Some(file) = cached
            .as_deref()
            .and_then(|cached| read_if(cached, describes))
        {
            return Some(file);
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(self.timeout)
            .build()
            .new_agent();
        for server in &self.servers {
            let url = format!("{}/buildid/{hex}/debuginfo", server.trim_end_matches('/'));
            let Ok(mut response) = agent.get(&url).call() else {
                continue;
            };
            let Ok(data) = response
                .body_mut()
                .with_config()
                .limit(DOWNLOAD_LIMIT)
                .read_to_vec()
            else {
                continue;
            };
            if !describes(&data) {
                continue;
            }
            let path = cached
                .as_deref()
                .filter(|cached| keep(cached, &data))
                .map_or_else(|| PathBuf::from(&url), Path::to_path_buf);
            return Some(DebugFile { path, data });
        }
        None
    }
}

/// Debuginfod's own cache: `DEBUGINFOD_CACHE_PATH`, or else under the
/// user's cache directory.
fn default_cache() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DEBUGINFOD_CACHE_PATH") {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|base| base.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("debuginfod_client"))
}

/// Writes a download to the cache whole or not at all, and returns whether
/// it is there.
fn keep(path: &Path, data: &[u8]) -> bool {
    let Some(directory) = path.parent() else {
        return false;
    };
    let partial = directory.join(format!(".debuginfo.{}.partial", std::process::id()));
    let kept = fs::create_dir_all(directory)
        .and_then(|()| fs::write(&partial, data))
        .and_then(|()| fs::rename(&partial, path));
    if kept.is_err() {
        let _ = fs::remove_file(&partial);
    }
    kept.is_ok()
}

/// Reads a file when `describes` accepts its contents.
fn read_if(path: &Path, describes: impl Fn(&[u8]) -> bool) -> Option<DebugFile> {
    let data = crate::image::backing::read_input(path).ok()?.0;
    describes(&data).then(|| DebugFile {
        path: path.to_path_buf(),
        data,
    })
}

/// Whether `data` is a debug file for the module `object`: an object for
/// its architecture with its build-id, or the checksum its link records.
fn debug_file_of(
    object: &object::File<'_>,
    data: &[u8],
    build_id: Option<&[u8]>,
    checksum: Option<u32>,
) -> bool {
    if checksum.is_some_and(|checksum| crc32(data) != checksum) {
        return false;
    }
    let Ok(debug) = object::File::parse(data) else {
        return false;
    };
    debug.architecture() == object.architecture()
        && build_id.is_none_or(|id| debug.build_id().ok().flatten() == Some(id))
}

/// Whether an object has DWARF of its own, rather than only the empty
/// sections a stripped file keeps.
pub fn has_dwarf(object: &object::File<'_>) -> bool {
    object
        .section_by_name(".debug_info")
        .and_then(|section| section.data().ok())
        .is_some_and(|data| !data.is_empty())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    })
}

/// The CRC-32 a `.gnu_debuglink` records: the IEEE polynomial, reflected,
/// as zlib computes it.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the table's indices are below 256"
)]
fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0_u32; 256];
        let mut index = 0;
        while index < 256 {
            let mut value = index as u32;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 1 == 0 {
                    value >> 1
                } else {
                    (value >> 1) ^ 0xedb8_8320
                };
                bit += 1;
            }
            table[index] = value;
            index += 1;
        }
        table
    };
    !data.iter().fold(!0_u32, |crc, &byte| {
        TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn debuglink_checksums_are_zlibs_crc32() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414f_a339
        );
    }
}
