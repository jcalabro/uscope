//! Source-level spellings of mangled linker names.

/// Bounds the C++ demangler's recursion so adversarial names cannot exhaust
/// the stack.
const CPP_RECURSION_LIMIT: u32 = 96;

/// Demangles a Rust (legacy or v0) or Itanium C++ linker name.
///
/// Rust is tried first because its legacy scheme is a subset of the Itanium
/// grammar; the alternate rendering omits Rust's per-crate hash suffix.
pub fn demangle(name: &str) -> Option<String> {
    if let Ok(demangled) = rustc_demangle::try_demangle(name) {
        return Some(format!("{demangled:#}"));
    }
    if !name.starts_with("_Z") {
        return None;
    }
    let symbol = cpp_demangle::Symbol::new_with_options(
        name,
        &cpp_demangle::ParseOptions::default().recursion_limit(CPP_RECURSION_LIMIT),
    )
    .ok()?;
    symbol
        .demangle_with_options(
            &cpp_demangle::DemangleOptions::new().recursion_limit(CPP_RECURSION_LIMIT),
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::demangle;

    #[test]
    fn rust_and_cpp_names_demangle_and_other_names_do_not() {
        for (mangled, expected) in [
            // Rust v0, as current toolchains emit it, without crate hashes.
            (
                "_RNvCsdHzz05DFzbQ_5crash9crash_now",
                Some("crash::crash_now"),
            ),
            // Rust legacy, without the trailing hash.
            (
                "_ZN4core3fmt5write17h0123456789abcdefE",
                Some("core::fmt::write"),
            ),
            ("_ZN3foo3barEv", Some("foo::bar()")),
            ("_ZNSt6thread6detachEv", Some("std::thread::detach()")),
            // GCC clones keep their suffix.
            ("_Z3bazi.constprop.0", Some("baz(int) [clone .constprop.0]")),
            ("main", None),
            ("_start", None),
            ("_Z", None),
            ("_ZZZZZZZZ", None),
        ] {
            assert_eq!(demangle(mangled).as_deref(), expected, "{mangled}");
        }
    }

    #[test]
    fn deeply_nested_cpp_names_are_rejected_instead_of_exhausting_the_stack() {
        let nested = format!("_Z1f{}i{}", "PFv".repeat(10_000), "E".repeat(10_000));
        assert_eq!(demangle(&nested), None);
    }
}
