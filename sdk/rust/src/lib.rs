//! Views for uscope, carried in a program's debug information.
//!
//! [`uscope_views_file!`] embeds a view file in the `.debug_uscope_views`
//! section of the object that expands it. uscope presents the module's own
//! types with those views, and no other module's. The section is not loaded
//! when the program runs, and `strip --strip-debug` removes it with the
//! rest of the debug information.

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
