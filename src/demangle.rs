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

/// Whether a mangled linker name demangles to a name a person writes. As
/// with gdb, the name may leave out the scopes that qualify it, and a C++
/// function's parameters: `scale`, `shapes::scale`, and
/// `shapes::scale(double)` all spell `_ZN6shapes5scaleEd`.
pub fn spells(mangled: &str, name: &str) -> bool {
    let (written, written_parameters) = split_parameters(name);
    // Only a mangled name holding the name's last part can demangle to it,
    // which spares demangling every symbol of a module.
    let last = written.rsplit("::").next().unwrap_or(written);
    if last.is_empty() || !mangled.contains(last) {
        return false;
    }
    let Some(demangled) = demangle(mangled) else {
        return false;
    };
    let (qualified, parameters) = split_parameters(&demangled);
    // A clone, such as `[clone .constprop.0]`, is other code.
    if parameters.contains("[clone") {
        return false;
    }
    let named = qualified == written
        || qualified
            .strip_suffix(written)
            .is_some_and(|scopes| scopes.ends_with("::"));
    named
        && (written_parameters.is_empty()
            || parameters == written_parameters
            || parameters
                .strip_prefix(written_parameters)
                .is_some_and(|qualifiers| qualifiers.starts_with(' ')))
}

/// A function's name and its parameter list, which begins at the first
/// parenthesis outside template arguments.
fn split_parameters(name: &str) -> (&str, &str) {
    let mut depth = 0_usize;
    for (index, character) in name.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            '(' if depth == 0 => return name.split_at(index),
            _ => {}
        }
    }
    (name, "")
}

#[cfg(test)]
mod tests {
    use super::{demangle, spells};

    #[test]
    fn written_names_spell_mangled_ones_with_or_without_scopes_and_parameters() {
        for (mangled, name, expected) in [
            ("_ZN6shapes5scaleEd", "shapes::scale", true),
            ("_ZN6shapes5scaleEd", "shapes::scale(double)", true),
            ("_ZN6shapes5scaleEd", "scale", true),
            ("_ZN6shapes5scaleEd", "shapes::scale(int)", false),
            ("_ZN6shapes5scaleEd", "apes::scale", false),
            ("_ZN6shapes5scaleEd", "shapes::scal", false),
            ("_ZNK6shapes6Widget4pickEv", "shapes::Widget::pick", true),
            ("_ZNK6shapes6Widget4pickEv", "Widget::pick()", true),
            ("_ZN5boxedIiE3getEv", "boxed<int>::get", true),
            ("_Z3bazi.constprop.0", "baz", false),
            (
                "_ZN4core3fmt5write17h0123456789abcdefE",
                "core::fmt::write",
                true,
            ),
            (
                "_RNvCsdHzz05DFzbQ_5crash9crash_now",
                "crash::crash_now",
                true,
            ),
            ("_RNvCsdHzz05DFzbQ_5crash9crash_now", "crash::crash", false),
            ("main", "main", false),
        ] {
            assert_eq!(spells(mangled, name), expected, "{mangled} as {name}");
        }
    }

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
