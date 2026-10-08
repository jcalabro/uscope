//! Rust's standard library, as the runtime that reports a program's
//! panics.
//!
//! std calls `__rustc::rust_panic(&mut dyn PanicPayload)` once the panic
//! hook has run and before unwinding begins, so a panic is reported there
//! before anything catches it. std ships no debug information for its
//! private types, so the payload's type is named by the symbol of its
//! `Display::fmt` in the payload's vtable, and its layout is a convention
//! of the pinned toolchain, checked by the tests against the program's own
//! panic hook:
//!
//! - `panic_handler::FormatStringPayload` holds `string: Option<String>`
//!   first, which every hook fills with the formatted message before
//!   `rust_panic` runs;
//! - `panic_handler::StaticStrPayload` is a `&'static str`;
//! - `begin_panic::Payload<A>` holds `Option<A>`, which the program's own
//!   debug information describes, as it instantiated it;
//! - `resume_unwind::RewrapBox` holds the `Box<dyn Any + Send>` a caught
//!   panic left, whose type the `Any` vtable's `type_id` names.
//!
//! A `String` is its capacity, pointer, and length, and `None` of an
//! `Option<String>` is the capacity no allocation can have.

use std::path::{Component, Path};
use std::sync::Arc;

use super::{
    Crossing, Partial, RuntimeException, RuntimeHook, RuntimeImage, RuntimeModel, RuntimeSignals,
    RuntimeStop, RuntimeTask, StoredValue, TaskContext, TaskPage, TaskRef, ThreadActivity,
};
use crate::unwind::RegisterFile;
use crate::{
    ExceptionFilter, ImageAddress, LanguageExceptionKind, StackSegment, ThreadId, VirtualAddress,
};

/// What Rust reports: every panic, as it begins.
pub const EXCEPTION_FILTERS: [ExceptionFilter; 1] = [PANIC];

const PANIC: ExceptionFilter = ExceptionFilter {
    id: "rust-panic",
    language: "Rust",
    kind: LanguageExceptionKind::Raised,
    label: "Rust panics",
    description: "Stop wherever a panic begins, before anything catches it, with the frame that \
                  panicked selected",
    default: true,
};

/// x86-64's DWARF numbers for the first two integer argument registers,
/// which pass the payload's data and vtable.
const RDI: u16 = 5;
const RSI: u16 = 4;
/// Where a trait object's vtable keeps its size, and its first method:
/// `Display::fmt` for a payload, `type_id` for an `Any`.
const VTABLE_SIZE: u64 = 8;
const VTABLE_FIRST_METHOD: u64 = 24;
/// `None` of an `Option<String>`: a capacity above `isize::MAX`.
const NO_STRING: u64 = 1 << 63;
/// The longest message read; a longer one is refused rather than cut.
const MAX_MESSAGE: u64 = 1 << 16;

/// The standard library's runtime, when the module carries it.
pub fn detect(image: &Arc<dyn RuntimeImage + Send + Sync>) -> Option<Arc<dyn RuntimeModel>> {
    let rust = image
        .producers()
        .iter()
        .any(|producer| producer.contains("(rustc version "));
    let panic = image.function_answering("__rustc::rust_panic")?;
    rust.then(|| {
        Arc::new(RustRuntime {
            image: Arc::clone(image),
            hooks: [RuntimeHook {
                filter: &PANIC,
                address: panic.address,
            }],
        }) as Arc<dyn RuntimeModel>
    })
}

#[derive(Debug)]
struct RustRuntime {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    hooks: [RuntimeHook; 1],
}

/// A panic's payload, as its type says to read it.
struct Payload<'a> {
    stop: &'a dyn RuntimeStop,
    image: &'a (dyn RuntimeImage + Send + Sync),
}

impl Payload<'_> {
    fn word(&self, address: u64) -> Result<u64, Arc<str>> {
        let mut bytes = [0; 8];
        self.stop
            .read(VirtualAddress::new(address), &mut bytes)
            .then(|| u64::from_le_bytes(bytes))
            .ok_or_else(|| format!("the panic's memory at {address:#x} is unreadable").into())
    }

    /// The demangled name of the method a vtable holds at `slot`.
    fn method(&self, vtable: u64, slot: u64) -> Result<Arc<str>, Arc<str>> {
        let method = self.word(vtable + slot)?;
        let address = method
            .checked_sub(self.stop.load_bias())
            .map(ImageAddress::new)
            .ok_or("the panic's vtable points outside the program")?;
        self.image
            .symbol_at(address)
            .ok_or_else(|| format!("no symbol names the method at {method:#x}").into())
    }

    fn text(&self, pointer: u64, length: u64) -> Result<String, Arc<str>> {
        if length > MAX_MESSAGE {
            return Err(format!("the message is {length} bytes long").into());
        }
        let mut bytes = vec![0; usize::try_from(length).map_err(|_| "a long message")?];
        if !self.stop.read(VirtualAddress::new(pointer), &mut bytes) {
            return Err(format!("the message at {pointer:#x} is unreadable").into());
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The `&str` at `at`.
    fn str(&self, at: u64) -> Result<String, Arc<str>> {
        self.text(self.word(at)?, self.word(at + 8)?)
    }

    /// The `Option<String>` at `at`.
    fn string(&self, at: u64) -> Result<Option<String>, Arc<str>> {
        let capacity = self.word(at)?;
        if capacity == NO_STRING {
            return Ok(None);
        }
        self.text(self.word(at + 8)?, self.word(at + 16)?).map(Some)
    }

    /// The message a value of type `ty` at `at` is, when it is text.
    fn message(&self, ty: &str, at: u64) -> Result<Option<String>, Arc<str>> {
        match ty {
            "&str" => self.str(at).map(Some),
            "alloc::string::String" => self.string(at),
            _ => Ok(None),
        }
    }

    /// What `rust_panic` was given, at `data` with vtable `vtable`.
    fn report(&self, data: u64, vtable: u64) -> Result<RuntimeException, Arc<str>> {
        let display = self.method(vtable, VTABLE_FIRST_METHOD)?;
        let ty = implementer(&display).ok_or_else(|| format!("the payload's `{display}`"))?;
        let message = |text: String| RuntimeException {
            message: format!("panicked: {text}").into(),
            value: None,
        };
        let path = ty.split('<').next().unwrap_or(ty);
        match path.rsplit("::").next() {
            Some("FormatStringPayload") => self
                .string(data)?
                .map(message)
                .ok_or_else(|| "no panic hook formatted the message".into()),
            Some("StaticStrPayload") => self.str(data).map(message),
            Some("Payload") => {
                let held = argument(ty).ok_or_else(|| format!("the payload is a `{ty}`"))?;
                // `Option<&str>` and `Option<String>` keep their tag in a
                // niche; any other `A` keeps one before the value.
                Ok(self.message(held, data)?.map_or_else(
                    || RuntimeException {
                        message: format!("panicked with a value of type {held}").into(),
                        value: Some(format!("(*({}*){data:#x}).inner", quoted(ty)).into()),
                    },
                    message,
                ))
            }
            Some("RewrapBox") => {
                let (inner, any) = (self.word(data)?, self.word(data + 8)?);
                let type_id = self.method(any, VTABLE_FIRST_METHOD)?;
                let held = implementer(&type_id)
                    .ok_or_else(|| format!("the resumed panic's `{type_id}`"))?;
                Ok(RuntimeException {
                    message: self
                        .message(held, inner)?
                        .map_or_else(
                            || format!("panic resumed with a value of type {held}"),
                            |text| format!("panic resumed: {text}"),
                        )
                        .into(),
                    value: None,
                })
            }
            _ => Err(format!("its payload is a `{ty}`, which uscope does not read").into()),
        }
    }
}

/// The type `<T as Trait>::method` implements a trait for.
fn implementer(method: &str) -> Option<&str> {
    let inner = method.strip_prefix('<')?;
    let mut depth = 0_usize;
    for (at, character) in inner.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            ' ' if depth == 0 && inner[at..].starts_with(" as ") => return Some(&inner[..at]),
            _ => {}
        }
    }
    None
}

/// A type's name as an expression writes it, its generic last segment in
/// backticks: ``std::panicking::begin_panic::`Payload<i32>` ``.
fn quoted(ty: &str) -> String {
    let generic = ty.find('<').unwrap_or(ty.len());
    let last = ty[..generic].rfind("::").map_or(0, |at| at + 2);
    format!("{}`{}`", &ty[..last], &ty[last..])
}

/// The one type argument of a generic type, such as `i32` of
/// `Payload<i32>`.
fn argument(ty: &str) -> Option<&str> {
    let (_, rest) = ty.split_once('<')?;
    rest.strip_suffix('>')
}

impl RuntimeModel for RustRuntime {
    fn tasks(
        &self,
        _stop: &dyn RuntimeStop,
        _start: u64,
        _limit: usize,
        _program_only: bool,
    ) -> Partial<TaskPage> {
        Partial {
            value: TaskPage {
                tasks: Vec::new(),
                next: None,
            },
            gaps: Vec::new(),
        }
    }

    fn thread_activity(&self, _stop: &dyn RuntimeStop, _thread: ThreadId) -> ThreadActivity {
        ThreadActivity::Idle
    }

    fn task_context(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        Ok(None)
    }

    fn thread_stacks(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>> {
        Ok(Vec::new())
    }

    fn cross(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
        _frame: &RegisterFile,
        _after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        Ok(Crossing::Stay)
    }

    fn signals(&self) -> RuntimeSignals {
        RuntimeSignals::default()
    }

    fn hooks(&self) -> &[RuntimeHook] {
        &self.hooks
    }

    fn exception(
        &self,
        stop: &dyn RuntimeStop,
        _hook: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        let argument = |register| {
            registers
                .get(register)
                .ok_or_else(|| Arc::<str>::from("the panic's payload is unavailable"))
        };
        let payload = Payload {
            stop,
            image: self.image.as_ref(),
        };
        let (data, vtable) = (argument(RDI)?, argument(RSI)?);
        // A payload's size bounds every read of it.
        let size = payload.word(vtable + VTABLE_SIZE)?;
        if size > 1 << 12 {
            return Err(format!("the payload claims to be {size} bytes").into());
        }
        payload.report(data, vtable)
    }

    fn dynamic_value(
        &self,
        _stop: &dyn RuntimeStop,
        _representation: &str,
        _value: StoredValue<'_>,
    ) -> Option<Result<super::DynamicValue, Arc<str>>> {
        None
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        None
    }

    fn moving_task(
        &self,
        _stop: &dyn RuntimeStop,
        _registers: &RegisterFile,
    ) -> Result<u64, Arc<str>> {
        Err("Rust's standard library moves no task".into())
    }

    fn task_stack(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>> {
        Ok(None)
    }

    fn call_out(
        &self,
        _entry: ImageAddress,
        _registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>> {
        None
    }

    fn task_starter(&self) -> Option<ImageAddress> {
        None
    }

    fn started_task(
        &self,
        _stop: &dyn RuntimeStop,
        _registers: &RegisterFile,
    ) -> Result<RuntimeTask, Arc<str>> {
        Err("Rust's standard library starts no task".into())
    }

    fn task_noun(&self) -> &'static str {
        "task"
    }

    fn own_source(&self, path: &Path) -> bool {
        standard_library(path)
    }
}

/// Whether a source file is the standard library's, wherever the toolchain
/// put it: `/rustc/<commit>/library/<crate>/…` for its own code, and
/// `…/rustlib/src/rust/library/<crate>/…` for generic code the program
/// instantiated.
fn standard_library(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts.iter().enumerate().any(|(at, part)| {
        let toolchain = match at.checked_sub(3).map(|start| &parts[start..at]) {
            Some([_, "rustc", commit]) => commit.len() == 40,
            Some(["rustlib", "src", "rust"]) => true,
            _ => at == 2 && parts[0] == "rustc" && parts[1].len() == 40,
        };
        *part == "library"
            && toolchain
            && parts.get(at + 1).is_some_and(|krate| {
                matches!(
                    *krate,
                    "core" | "alloc" | "std" | "panic_unwind" | "panic_abort"
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_name_the_types_they_implement_for() {
        assert_eq!(
            implementer(
                "<std::panicking::begin_panic::Payload<&str> as core::panic::PanicPayload>::get"
            ),
            Some("std::panicking::begin_panic::Payload<&str>")
        );
        assert_eq!(
            implementer("<alloc::string::String as core::any::Any>::type_id"),
            Some("alloc::string::String")
        );
        assert_eq!(implementer("core::fmt::write"), None);
        assert_eq!(
            argument("std::panicking::begin_panic::Payload<i32>"),
            Some("i32")
        );
        assert_eq!(
            quoted("std::panicking::begin_panic::Payload<alloc::string::String>"),
            "std::panicking::begin_panic::`Payload<alloc::string::String>`"
        );
    }

    #[test]
    fn the_standard_library_is_its_sources_under_the_toolchain() {
        let runtime = |path: &str| standard_library(Path::new(path));
        assert!(runtime(
            "/rustc/375b1431b7d89d1c2e2bc168c011848ae12b7d14/library/core/src/option.rs"
        ));
        assert!(runtime(
            "/nix/store/x-rust-default/lib/rustlib/src/rust/library/std/src/panicking.rs"
        ));
        assert!(!runtime("/home/me/project/library/core/src/main.rs"));
        assert!(!runtime("tests/fixtures/rust/tokio/panics/src/main.rs"));
    }
}
