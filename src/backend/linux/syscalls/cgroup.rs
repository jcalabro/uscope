//! A cgroup to trace, named by its path or by the id of the container it
//! holds, and the pid namespace its processes see.

use std::path::{Path, PathBuf};

/// Where the unified hierarchy is mounted.
pub const ROOT: &str = "/sys/fs/cgroup";

/// The proc inode numbers of the initial cgroup and pid namespaces, which
/// the kernel fixes.
const INIT_CGROUP_NS: &str = "cgroup:[4026531835]";

/// A cgroup the kernel's programs recognize a process by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgroup {
    pub path: PathBuf,
    /// The kernel's id for it, its directory's inode.
    pub id: u64,
    /// Its depth below the root, which is level 0.
    pub level: u32,
}

/// A pid namespace to show ids in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidNamespace {
    /// Its proc inode number.
    pub inum: u32,
    /// Its depth below the initial namespace, which is level 0.
    pub level: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CgroupError {
    #[error("{0} is not a cgroup below {ROOT}")]
    NotCgroup(PathBuf),
    #[error("{ROOT} is not the unified (v2) cgroup hierarchy, which tracing a cgroup needs")]
    NotUnified,
    #[error(
        "uscope runs in a cgroup namespace of its own, where it cannot tell how deep a cgroup \
         is; run it in the host's (with docker, --cgroupns=host)"
    )]
    Namespaced,
    #[error("a container id is at least 12 hexadecimal digits, not {0:?}")]
    BadId(String),
    #[error("no cgroup below {ROOT} holds a container {0}")]
    NoContainer(String),
    #[error("{count} cgroups hold a container whose id begins {id}; give more of it")]
    Ambiguous { id: String, count: usize },
    #[error("reading {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
}

impl Cgroup {
    /// The cgroup at `path`, in the host's unified hierarchy.
    ///
    /// # Errors
    ///
    /// When `path` is no cgroup there, when the hierarchy is not unified,
    /// or when uscope is in a cgroup namespace of its own.
    pub fn open(path: &Path) -> Result<Self, CgroupError> {
        let unified = nix::sys::statfs::statfs(ROOT)
            .is_ok_and(|fs| fs.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC);
        if !unified {
            return Err(CgroupError::NotUnified);
        }
        let namespace = std::fs::read_link("/proc/self/ns/cgroup")
            .map_err(|error| CgroupError::Io("/proc/self/ns/cgroup".into(), error))?;
        if namespace != Path::new(INIT_CGROUP_NS) {
            return Err(CgroupError::Namespaced);
        }
        below(Path::new(ROOT), path)
    }

    /// The cgroup of the container whose id begins `id`.
    ///
    /// # Errors
    ///
    /// As [`Cgroup::open`]'s, and when no cgroup, or more than one, holds
    /// such a container.
    pub fn container(id: &str) -> Result<Self, CgroupError> {
        Self::open(&container(Path::new(ROOT), id)?)
    }

    /// The processes in it now, by their ids on the host.
    #[must_use]
    pub fn processes(&self) -> Vec<u32> {
        std::fs::read_to_string(self.path.join("cgroup.procs"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect()
    }

    /// Whether it is still there.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.join("cgroup.procs").exists()
    }
}

/// The cgroup at `path` in the hierarchy mounted at `root`.
fn below(root: &Path, path: &Path) -> Result<Cgroup, CgroupError> {
    use std::os::unix::fs::MetadataExt as _;
    let not = || CgroupError::NotCgroup(path.to_owned());
    let path = path.canonicalize().map_err(|_| not())?;
    let root = root
        .canonicalize()
        .map_err(|error| CgroupError::Io(root.to_owned(), error))?;
    let relative = path.strip_prefix(&root).map_err(|_| not())?;
    if !path.join("cgroup.procs").is_file() {
        return Err(not());
    }
    let id = std::fs::metadata(&path)
        .map_err(|error| CgroupError::Io(path.clone(), error))?
        .ino();
    let level = u32::try_from(relative.components().count()).map_err(|_| not())?;
    Ok(Cgroup { path, id, level })
}

/// The cgroup below `root` that holds the container whose id begins `id`,
/// as docker, podman, containerd, and CRI-O name theirs under systemd, or
/// as a directory of the id alone under cgroupfs.
fn container(root: &Path, id: &str) -> Result<PathBuf, CgroupError> {
    if id.len() < 12 || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CgroupError::BadId(id.to_owned()));
    }
    let id = id.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut pending = vec![(root.to_owned(), 0)];
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let bare = ["docker-", "libpod-", "cri-containerd-", "crio-"]
                .iter()
                .find_map(|prefix| name.strip_prefix(prefix)?.strip_suffix(".scope"))
                .unwrap_or(&name);
            if bare.len() == 64 && bare.starts_with(&id) {
                found.push(entry.path());
            } else if depth < 16 {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    match found.len() {
        0 => Err(CgroupError::NoContainer(id)),
        1 => Ok(found.remove(0)),
        count => Err(CgroupError::Ambiguous { id, count }),
    }
}

/// The pid namespace `processes` share, when it is not uscope's own.
///
/// # Errors
///
/// Why ids must be shown as the host sees them: there are no processes to
/// ask, they are in different namespaces, or theirs cannot be read.
pub fn pid_namespace(processes: &[u32]) -> Result<Option<PidNamespace>, String> {
    let own = namespace_of("self").map_err(|error| format!("uscope's own: {error}"))?;
    let mut shared = None;
    for &process in processes {
        let theirs = match namespace_of(&process.to_string()) {
            Ok(theirs) => theirs,
            // It ended as it was read.
            Err(_) if !Path::new(&format!("/proc/{process}")).exists() => continue,
            Err(error) => return Err(format!("process {process}'s: {error}")),
        };
        match shared {
            None => shared = Some(theirs),
            Some(seen) if seen != theirs => {
                return Err("its processes are in different pid namespaces".into());
            }
            Some(_) => {}
        }
    }
    let Some(shared) = shared else {
        return Err("it had no processes as tracing began".into());
    };
    Ok((shared != own).then_some(shared))
}

/// The pid namespace of `/proc/{process}`.
fn namespace_of(process: &str) -> Result<PidNamespace, std::io::Error> {
    let link = std::fs::read_link(format!("/proc/{process}/ns/pid"))?;
    let inum = link
        .to_str()
        .and_then(|text| text.strip_prefix("pid:[")?.strip_suffix(']'))
        .and_then(|number| number.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("unexpected link {}", link.display())))?;
    // Its ids from the initial namespace inward, one per level.
    let status = std::fs::read_to_string(format!("/proc/{process}/status"))?;
    let depth = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map_or(1, |ids| ids.split_whitespace().count());
    Ok(PidNamespace {
        inum,
        level: own_level()? + u32::try_from(depth.saturating_sub(1)).unwrap_or(0),
    })
}

/// How deep uscope's own pid namespace is, which `NSpid` lines count from.
fn own_level() -> Result<u32, std::io::Error> {
    // The initial namespace's inode number is fixed; any other one's depth
    // cannot be read from inside it, so uscope assumes the host's.
    let link = std::fs::read_link("/proc/self/ns/pid")?;
    if link == Path::new("pid:[4026531836]") {
        Ok(0)
    } else {
        Err(std::io::Error::other(
            "uscope runs in a pid namespace of its own",
        ))
    }
}

/// The id `process` (as the host knows it) has in a namespace at `level`.
#[must_use]
pub fn id_in(process: u32, level: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{process}/status")).ok()?;
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))?;
    ids.split_whitespace().nth(level as usize)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_is_found_by_any_runtime_s_name_for_its_cgroup() {
        let root = std::env::temp_dir().join(format!("uscope-cgroups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let a = "a".repeat(64);
        let b = format!("ab{}", "c".repeat(62));
        let systemd = root.join(format!("system.slice/docker-{a}.scope"));
        let cgroupfs = root.join(format!("kubepods/besteffort/pod1/{b}"));
        for directory in [&systemd, &cgroupfs] {
            std::fs::create_dir_all(directory).expect("make the tree");
            std::fs::write(directory.join("cgroup.procs"), "").expect("procs");
        }

        assert_eq!(container(&root, &a[..12]).expect("found"), systemd);
        assert_eq!(
            container(&root, &b.to_uppercase()).expect("found"),
            cgroupfs
        );
        assert!(matches!(
            container(&root, "aaaaaaaaaaab"),
            Err(CgroupError::NoContainer(_))
        ));
        assert!(matches!(
            container(&root, "abc"),
            Err(CgroupError::BadId(_))
        ));
        std::fs::create_dir_all(root.join(format!("other/crio-{}.scope", "a".repeat(64))))
            .expect("make a twin");
        assert!(matches!(
            container(&root, &a[..12]),
            Err(CgroupError::Ambiguous { count: 2, .. })
        ));

        let found = below(&root, &cgroupfs).expect("a cgroup");
        assert_eq!(found.level, 4);
        assert!(matches!(
            below(&root, &root.join("kubepods/besteffort/pod1/missing")),
            Err(CgroupError::NotCgroup(_))
        ));
        // A directory without cgroup.procs is not one.
        assert!(matches!(
            below(&root, &root.join("kubepods")),
            Err(CgroupError::NotCgroup(_))
        ));
        std::fs::remove_dir_all(&root).expect("clean up");
    }
}
