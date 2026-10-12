//! System call names and numbers for each ABI, generated from the kernel's
//! UAPI headers into `table.gen.rs`.

use super::model::{Abi, SyscallId};

mod generated {
    include!("table.gen.rs");
}

/// Every call of an ABI, as (number, name), in ascending number order.
#[must_use]
pub const fn calls(abi: Abi) -> &'static [(u32, &'static str)] {
    match abi {
        Abi::X86_64 => generated::X86_64,
        Abi::X86_32 => generated::X86_32,
        Abi::X32 => generated::X32,
    }
}

/// The bit x32 calls set in their numbers, which the tables leave out.
pub const X32_SYSCALL_BIT: u32 = 0x4000_0000;

pub(super) fn name(id: SyscallId) -> Option<&'static str> {
    let calls = calls(id.abi);
    calls
        .binary_search_by_key(&id.number, |&(number, _)| number)
        .ok()
        .map(|index| calls[index].1)
}

pub(super) fn number(abi: Abi, name: &str) -> Option<u32> {
    calls(abi)
        .iter()
        .find(|&&(_, candidate)| candidate == name)
        .map(|&(number, _)| number)
}

/// The highest number any ABI's table lists, plus one: the size of a
/// bitmap over one ABI's calls.
#[must_use]
pub fn limit(abi: Abi) -> u32 {
    calls(abi).last().map_or(0, |&(number, _)| number + 1)
}

/// Every call in every ABI.
pub fn all() -> impl Iterator<Item = SyscallId> {
    Abi::ALL.into_iter().flat_map(|abi| {
        calls(abi)
            .iter()
            .map(move |&(number, _)| SyscallId::new(abi, number))
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use super::*;

    const GENERATED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/syscalls/table.gen.rs");

    /// The kernel UAPI headers the flake provides, among the BPF include
    /// directories.
    fn uapi() -> std::path::PathBuf {
        let include = std::env::var("USCOPE_BPF_INCLUDE")
            .expect("USCOPE_BPF_INCLUDE names the kernel headers; run in the dev shell");
        include
            .split(':')
            .map(Path::new)
            .find(|path| path.join("asm/unistd_64.h").exists())
            .expect("one USCOPE_BPF_INCLUDE directory holds asm/unistd_64.h")
            .to_owned()
    }

    /// Reads `#define __NR_name number` lines, with x32's numbers written as
    /// `(__X32_SYSCALL_BIT + number)`.
    fn parse(header: &str) -> Vec<(u32, String)> {
        let mut calls: Vec<(u32, String)> = header
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("#define __NR_")?;
                let (name, value) = rest.split_once(' ')?;
                let value = value.trim();
                let value = value
                    .strip_prefix("(__X32_SYSCALL_BIT + ")
                    .and_then(|value| value.strip_suffix(')'))
                    .unwrap_or(value);
                Some((value.parse().ok()?, name.to_owned()))
            })
            .collect();
        calls.sort();
        calls
    }

    fn generate() -> String {
        let headers = uapi().join("asm");
        let mut text = String::from(
            "// Generated from the kernel's asm/unistd_{64,32,x32}.h by\n\
             // `syscalls::table::tests::tables_match_the_kernel_headers`; do not edit.\n\
             // Rerun it with USCOPE_UPDATE_SYSCALLS=1 after the headers change.\n",
        );
        for (constant, file) in [
            ("X86_64", "unistd_64.h"),
            ("X86_32", "unistd_32.h"),
            ("X32", "unistd_x32.h"),
        ] {
            let header = std::fs::read_to_string(headers.join(file)).expect("read a header");
            let calls = parse(&header);
            assert!(calls.len() > 300, "{file} lists the calls");
            writeln!(text, "\npub(super) const {constant}: &[(u32, &str)] = &[").expect("write");
            for (number, name) in calls {
                writeln!(text, "    ({number}, {name:?}),").expect("write");
            }
            text.push_str("];\n");
        }
        text
    }

    /// Fails when `table.gen.rs` no longer matches the pinned kernel
    /// headers. Set `USCOPE_UPDATE_SYSCALLS=1` to rewrite it.
    #[test]
    fn tables_match_the_kernel_headers() {
        let expected = generate();
        if std::env::var_os("USCOPE_UPDATE_SYSCALLS").is_some() {
            std::fs::write(GENERATED, &expected).expect("write the generated tables");
            return;
        }
        let actual = std::fs::read_to_string(GENERATED).unwrap_or_default();
        assert!(
            actual == expected,
            "src/syscalls/table.gen.rs is stale; rerun with USCOPE_UPDATE_SYSCALLS=1"
        );
    }

    #[test]
    fn names_and_numbers_round_trip_in_each_abi() {
        let openat = SyscallId::named(Abi::X86_64, "openat").expect("openat");
        assert_eq!(openat.number, 257);
        assert_eq!(
            SyscallId::named(Abi::X86_32, "openat").map(|id| id.number),
            Some(295)
        );
        assert_eq!(
            SyscallId::named(Abi::X32, "openat").map(|id| id.number),
            Some(257)
        );
        assert_eq!(SyscallId::new(Abi::X86_32, 1).name(), Some("exit"));
        assert_eq!(
            SyscallId::new(Abi::X86_64, 9999).display_name(),
            "syscall_0x270f"
        );
    }
}
