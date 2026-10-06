//! The values of a tree whose nodes keep their children in a list, each
//! node before its children, for the view of `Tree` in main.views. Its
//! arguments are where the root is kept and where a node keeps its first
//! child and its next sibling; it yields each node's address.

#![no_std]

use uscope_views::kernel::{arguments, emit, load};

/// The deepest tree it walks: a node waits here for each ancestor whose
/// next sibling is still to come.
const MAX_DEPTH: usize = 64;

/// Walks the tree.
///
/// # Safety
///
/// uscope calls it with `count` words at `pointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn run(pointer: *const u64, count: i32) -> i32 {
    // SAFETY: as the caller promises.
    let arguments = unsafe { arguments(pointer, count) };
    let &[root, child, sibling] = arguments else {
        return 1;
    };
    let mut waiting = [0_u64; MAX_DEPTH];
    let mut depth = 0;
    let mut node: u64 = load(root);
    while node != 0 {
        if !emit(&[node]) {
            return 0;
        }
        let first: u64 = load(node.wrapping_add(child));
        let next: u64 = load(node.wrapping_add(sibling));
        if first == 0 {
            node = next;
        } else {
            if next != 0 {
                if depth == MAX_DEPTH {
                    return 2;
                }
                waiting[depth] = next;
                depth += 1;
            }
            node = first;
        }
        if node == 0 && depth > 0 {
            depth -= 1;
            node = waiting[depth];
        }
    }
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
