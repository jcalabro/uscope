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
//!
//! dwz moves the debug information several files share into a supplementary
//! file, which each names by `.gnu_debugaltlink`, a path and the build-id
//! the file must have, or by DWARF 5's `.debug_sup`, a path and a checksum
//! the file's own `.debug_sup` records. It is found as gdb finds it: at that
//! path, relative to the real directory of the file naming it; by its
//! identifier under each debug directory's `.build-id`; under each debug
//! directory's `.dwz` when the path lies under one; and from a debuginfod
//! server.

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

/// The dwz supplementary file some debug information shares.
pub enum Supplementary {
    /// The debug information names none.
    None,
    Found(DebugFile),
    /// One is named but cannot be read, for the reason given.
    Missing(String),
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
        if let Some(file) = build_id.and_then(|id| self.by_build_id(id, describes)) {
            return Some(file);
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
        build_id.and_then(|id| self.download(id, describes))
    }

    /// The dwz supplementary file whose debug information the file at
    /// `path`, `object`, shares.
    pub fn supplementary(&self, path: &Path, object: &object::File<'_>) -> Supplementary {
        let link = match supplementary_link(object) {
            Ok(Some(link)) => link,
            Ok(None) => return Supplementary::None,
            Err(reason) => return Supplementary::Missing(reason),
        };
        let name = Path::new(OsStr::from_bytes(&link.name));
        let build_id = link.id.as_slice();
        let named = if link.standard {
            format!(
                "its supplementary file {} (checksum {})",
                name.display(),
                hex(build_id)
            )
        } else {
            format!(
                "its dwz supplementary file {} (build-id {})",
                name.display(),
                hex(build_id)
            )
        };
        if build_id.len() < 2 {
            return Supplementary::Missing(format!("{named} has no identifier to check"));
        }
        let describes = |data: &[u8]| supplementary_file_of(object, data, &link);
        let named_path = if name.as_os_str().is_empty() {
            None
        } else if name.is_absolute() {
            Some(name.to_path_buf())
        } else {
            // Beside the real file: a build-id entry links to it from
            // elsewhere.
            fs::canonicalize(path)
                .ok()
                .and_then(|path| Some(path.parent()?.join(name)))
        };
        let found = named_path
            .and_then(|path| read_if(&path, describes))
            .or_else(|| self.by_build_id(build_id, describes))
            .or_else(|| {
                // A distribution's path, as `/usr/lib/debug/.dwz/NAME`, under
                // each debug directory.
                let within = name.to_str()?.split_once("/.dwz/")?.1;
                self.directories
                    .iter()
                    .find_map(|directory| read_if(&directory.join(".dwz").join(within), describes))
            })
            .or_else(|| self.download(build_id, describes));
        found.map_or_else(
            || Supplementary::Missing(format!("{named} was not found")),
            Supplementary::Found,
        )
    }

    /// The file a build-id names under the debug directories that
    /// `describes` accepts.
    fn by_build_id(
        &self,
        build_id: &[u8],
        describes: impl Fn(&[u8]) -> bool + Copy,
    ) -> Option<DebugFile> {
        let hex = hex(build_id);
        self.directories.iter().find_map(|directory| {
            let candidate = directory
                .join(".build-id")
                .join(&hex[..2])
                .join(format!("{}.debug", &hex[2..]));
            read_if(&candidate, describes)
        })
    }

    /// Asks the debuginfod servers for a build-id's debug file that
    /// `describes` accepts, which is first looked for in the cache, and kept
    /// there once downloaded.
    fn download(
        &self,
        build_id: &[u8],
        describes: impl Fn(&[u8]) -> bool + Copy,
    ) -> Option<DebugFile> {
        if self.servers.is_empty() {
            return None;
        }
        let hex = hex(build_id);
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

/// How DWARF names its supplementary file: by a path and the identifier
/// the file must have.
struct SupplementaryLink {
    name: Vec<u8>,
    id: Vec<u8>,
    /// Whether DWARF 5's `.debug_sup` names it, by a checksum the file's
    /// own `.debug_sup` records, rather than `.gnu_debugaltlink` by its
    /// build-id.
    standard: bool,
}

/// The supplementary file `object`'s DWARF names, if any: by
/// `.gnu_debugaltlink`, else by `.debug_sup`, as gdb looks for them.
fn supplementary_link(object: &object::File<'_>) -> Result<Option<SupplementaryLink>, String> {
    if let Some((name, id)) = object
        .gnu_debugaltlink()
        .map_err(|error| error.to_string())?
    {
        return Ok(Some(SupplementaryLink {
            name: name.to_vec(),
            id: id.to_vec(),
            standard: false,
        }));
    }
    // A supplementary file names none.
    Ok(debug_sup(object)?
        .filter(|sup| !sup.supplementary)
        .map(|sup| SupplementaryLink {
            name: sup.name,
            id: sup.checksum,
            standard: true,
        }))
}

/// A `.debug_sup` section (DWARF 5, section 7.3.6).
struct DebugSup {
    /// Whether the file holding it is itself a supplementary file.
    supplementary: bool,
    /// The supplementary file's name, empty in a supplementary file.
    name: Vec<u8>,
    /// What identifies the supplementary file.
    checksum: Vec<u8>,
}

/// The object's `.debug_sup`, if it has one.
fn debug_sup(object: &object::File<'_>) -> Result<Option<DebugSup>, String> {
    use gimli::Reader as _;
    // By its exact name: looking one up by name that is missing builds the
    // name of its `.zdebug` form, an allocation in every load.
    let Some(section) = object
        .sections()
        .find(|section| section.name_bytes() == Ok(b".debug_sup"))
    else {
        return Ok(None);
    };
    let malformed = |error: &dyn std::fmt::Display| format!("its .debug_sup is malformed: {error}");
    let data = section
        .uncompressed_data()
        .map_err(|error| malformed(&error))?;
    let endian = if object.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };
    let mut reader = gimli::EndianSlice::new(&data, endian);
    let read = |reader: &mut gimli::EndianSlice<'_, gimli::RunTimeEndian>| {
        let version = reader.read_u16()?;
        let supplementary = reader.read_u8()?;
        let name = reader.read_null_terminated_slice()?;
        let length = reader.read_uleb128()?;
        let checksum =
            reader.split(usize::try_from(length).map_err(|_| gimli::Error::BadUnsignedLeb128)?)?;
        Ok::<_, gimli::Error>((
            version,
            supplementary,
            name.slice().to_vec(),
            checksum.slice().to_vec(),
        ))
    };
    let (version, supplementary, name, checksum) =
        read(&mut reader).map_err(|error| malformed(&error))?;
    if version != 5 {
        return Err(format!(
            "its .debug_sup has version {version}, which uscope does not read"
        ));
    }
    Ok(Some(DebugSup {
        supplementary: supplementary != 0,
        name,
        checksum,
    }))
}

/// Whether `data` is the supplementary file `link` names for `object`: an
/// object for its architecture with the build-id `.gnu_debugaltlink`
/// records, or, for `.debug_sup`, a supplementary file whose own
/// `.debug_sup` records the same checksum or whose build-id it is.
fn supplementary_file_of(object: &object::File<'_>, data: &[u8], link: &SupplementaryLink) -> bool {
    let Ok(candidate) = object::File::parse(data) else {
        return false;
    };
    if candidate.architecture() != object.architecture() {
        return false;
    }
    let build_id = candidate.build_id().ok().flatten();
    if !link.standard {
        return build_id == Some(link.id.as_slice());
    }
    build_id == Some(link.id.as_slice())
        || debug_sup(&candidate)
            .ok()
            .flatten()
            .is_some_and(|sup| sup.supplementary && sup.checksum == link.id)
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
