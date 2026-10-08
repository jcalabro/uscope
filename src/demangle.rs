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
    let last = last_part(name);
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

/// The part of a written or demangled name after its last scope, without
/// its parameters, which a name `spells` must share with its symbol's.
pub fn last_part(name: &str) -> &str {
    let (written, _) = split_parameters(name);
    written.rsplit("::").next().unwrap_or(written)
}

/// A demangled Rust function's path, with no generic arguments and an
/// inherent method's type unwrapped: v0's
/// `<tokio::runtime::park::CachedParkThread>::block_on::<F>` and legacy's
/// `tokio::runtime::park::CachedParkThread::block_on` are both the path
/// `tokio::runtime::park::CachedParkThread::block_on`. `None` for a trait
/// implementation's method, which a path does not name, or unbalanced
/// brackets.
pub fn rust_path(demangled: &str) -> Option<String> {
    let unwrapped;
    let mut name = demangled;
    if let Some(inner) = name.strip_prefix('<') {
        let close = closing(inner)?;
        if inner[..close].contains(" as ") {
            return None;
        }
        unwrapped = format!("{}{}", &inner[..close], &inner[close + 1..]);
        name = &unwrapped;
    }
    let mut path = String::with_capacity(name.len());
    let mut depth = 0_usize;
    for (at, character) in name.char_indices() {
        match character {
            // A function type's arrow closes nothing.
            '>' if name[..at].ends_with('-') => {}
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            _ if depth == 0 => path.push(character),
            _ => {}
        }
    }
    (depth == 0).then(|| path.replace("::::", "::").trim_end_matches("::").to_owned())
}

/// Where the `>` closing a `<` just before `text` is in it.
fn closing(text: &str) -> Option<usize> {
    let mut depth = 1_usize;
    for (at, character) in text.char_indices() {
        match character {
            '>' if text[..at].ends_with('-') => {}
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
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
    use super::{demangle, rust_path, spells};

    #[test]
    fn rust_paths_leave_out_generic_arguments_and_inherent_impls_brackets() {
        let path = |mangled: &str| rust_path(&demangle(mangled).expect("a Rust name"));
        let block_on = "tokio::runtime::park::CachedParkThread::block_on";
        for (mangled, expected) in [
            (
                "_RINvMs2_NtNtCshWOCllL2uKN_5tokio7runtime4parkNtB6_16CachedParkThread\
                 8block_onNCNvCsiiWaAJDmxH2_7workers3run0EB1h_",
                Some(block_on.to_owned()),
            ),
            (
                "_RNCINvMs2_NtNtCshWOCllL2uKN_5tokio7runtime4parkNtB8_16CachedParkThread\
                 8block_onNCNvCsiiWaAJDmxH2_7workers3run0E0B1j_",
                Some(format!("{block_on}::{{closure#0}}")),
            ),
            (
                "_ZN5tokio7runtime4park16CachedParkThread8block_on17h0123456789abcdefE",
                Some(block_on.to_owned()),
            ),
        ] {
            assert_eq!(path(mangled), expected, "{mangled}");
        }
        assert_eq!(
            rust_path("<alloc::boxed::Box<F> as core::future::Future>::poll"),
            None
        );
        assert_eq!(
            rust_path("core::ops::function::FnOnce<fn() -> u8>::call"),
            Some("core::ops::function::FnOnce::call".into())
        );
        assert_eq!(rust_path("a::b<c"), None);
    }

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
