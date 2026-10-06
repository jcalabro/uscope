//! Views for uscope, carried in a program's debug information, and kernels
//! for them, written in Rust.
//!
//! [`uscope_views_file!`] embeds a view file in the `.debug_uscope_views`
//! section of the object that expands it. uscope presents the module's own
//! types with those views, and no other module's. [`uscope_kernel!`]
//! embeds a kernel those views may call, with its source. The section is
//! not loaded when the program runs, and `strip --strip-debug` removes it
//! with the rest of the debug information.
//!
//! The `kernel` module, built for `wasm32-unknown-unknown`, writes a
//! kernel itself.

#![no_std]

/// Embeds the view file at `path`, which the assembler reads: relative to
/// the directory the compiler runs in, so usually written from
/// `CARGO_MANIFEST_DIR`.
///
/// ```ignore
/// uscope_views::uscope_views_file!(concat!(env!("CARGO_MANIFEST_DIR"), "/app.views"));
/// ```
#[macro_export]
macro_rules! uscope_views_file {
    ($path:expr) => {
        // A record: kind 1 (view text), format 1, its length, then the
        // file.
        ::core::arch::global_asm!(
            ".pushsection .debug_uscope_views,\"\",@progbits",
            ".byte 1, 1",
            ".long 8f - 7f",
            "7:",
            concat!(".incbin \"", $path, "\""),
            "8:",
            ".popsection",
        );
    };
}

/// Embeds the kernel `name`, whose module is the file at `module` and
/// whose source is the file at `source`, so that it is reviewed as that.
/// The assembler reads both, as [`uscope_views_file!`] says.
///
/// ```ignore
/// uscope_views::uscope_kernel!("tree", concat!(env!("CARGO_MANIFEST_DIR"), "/kernel/src/lib.rs"), env!("TREE_KERNEL"));
/// ```
#[macro_export]
macro_rules! uscope_kernel {
    ($name:expr, $source:expr, $module:expr) => {
        // A record: kind 2 (kernel), format 1, its length, then the name's
        // length and the name, the source's length and the source, and the
        // module.
        ::core::arch::global_asm!(
            ".pushsection .debug_uscope_views,\"\",@progbits",
            ".byte 2, 1",
            ".long 8f - 7f",
            "7:",
            ".short 4f - 3f",
            "3:",
            concat!(".ascii \"", $name, "\""),
            "4:",
            ".long 6f - 5f",
            "5:",
            concat!(".incbin \"", $source, "\""),
            "6:",
            concat!(".incbin \"", $module, "\""),
            "8:",
            ".popsection",
        );
    };
}

/// Writing a kernel: a WebAssembly function a view calls to walk a
/// structure that is an algorithm more than a layout, as uscope's
/// `docs/views.md` describes. It exports `run`, which takes the view's
/// arguments as 64-bit words, and yields items of words, such as
/// addresses, which the view presents:
///
/// ```ignore
/// #[unsafe(no_mangle)]
/// pub unsafe extern "C" fn run(pointer: *const u64, count: i32) -> i32 {
///     // SAFETY: uscope passes `count` words.
///     let arguments = unsafe { uscope_views::kernel::arguments(pointer, count) };
///     let &[head] = arguments else {
///         return 1;
///     };
///     let mut node = head;
///     while node != 0 && uscope_views::kernel::emit(&[node]) {
///         node = uscope_views::kernel::load(node);
///     }
///     0
/// }
/// ```
///
/// Build it as a `cdylib` for `wasm32-unknown-unknown`, `no_std`, with
/// `panic = "abort"`. `run` returns 0 when it is done, and anything else
/// says it failed.
#[cfg(target_family = "wasm")]
pub mod kernel {
    #[link(wasm_import_module = "uscope_kernel_v1")]
    unsafe extern "C" {
        #[link_name = "read"]
        fn host_read(address: u64, buffer: *mut u8, length: i32) -> i32;
        #[link_name = "yield"]
        fn host_yield(words: *const u64, count: i32) -> i32;
    }

    /// The view's arguments.
    ///
    /// # Safety
    ///
    /// `arguments` and `count` are those `run` was called with.
    #[must_use]
    pub unsafe fn arguments<'a>(arguments: *const u64, count: i32) -> &'a [u64] {
        let count = usize::try_from(count).unwrap_or(0);
        // SAFETY: uscope passes `count` words at `arguments`.
        unsafe { core::slice::from_raw_parts(arguments, count) }
    }

    /// Fills `buffer` with the program's memory at `address`. A read uscope
    /// cannot make ends the run, so a kernel never sees one fail.
    pub fn read(address: u64, buffer: &mut [u8]) {
        let length = i32::try_from(buffer.len()).unwrap_or(i32::MAX);
        // SAFETY: the host writes at most `length` bytes into `buffer`.
        unsafe { host_read(address, buffer.as_mut_ptr(), length) };
    }

    /// An integer the program's memory holds.
    pub trait Integer: Sized {
        fn from_le(bytes: &[u8]) -> Self;
    }

    macro_rules! integers {
        ($($ty:ty),*) => {$(
            impl Integer for $ty {
                fn from_le(bytes: &[u8]) -> Self {
                    let mut le = [0; core::mem::size_of::<$ty>()];
                    le.copy_from_slice(bytes);
                    <$ty>::from_le_bytes(le)
                }
            }
        )*};
    }

    integers!(u8, u16, u32, u64, i8, i16, i32, i64);

    /// The integer at `address` in the program's memory.
    #[must_use]
    pub fn load<T: Integer>(address: u64) -> T {
        let mut bytes = [0; 8];
        let bytes = &mut bytes[..core::mem::size_of::<T>()];
        read(address, bytes);
        T::from_le(bytes)
    }

    /// Yields an item, and returns whether uscope wants another. uscope
    /// may also stop running the kernel at any call.
    #[must_use]
    pub fn emit(words: &[u64]) -> bool {
        let count = i32::try_from(words.len()).unwrap_or(i32::MAX);
        // SAFETY: the host reads `count` words at `words`.
        unsafe { host_yield(words.as_ptr(), count) != 0 }
    }
}
