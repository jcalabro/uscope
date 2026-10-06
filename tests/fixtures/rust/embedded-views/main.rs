//! A Rust program that carries views for its own types in its
//! `.debug_uscope_views` section, and a kernel one of them calls, through
//! the SDK's macros.

use std::hint::black_box;

uscope_views::uscope_views_file!("tests/fixtures/rust/embedded-views/main.views");

/// Tags, which their view presents as the names they hold.
pub struct Tags {
    names: Vec<&'static str>,
}

/// A temperature in degrees Celsius.
pub struct Celsius(f64);

/// A tree whose nodes keep their children in a list.
pub struct Tree {
    root: Option<Box<Node>>,
    count: usize,
}

/// A node of a tree, whose links only the kernel follows.
#[expect(dead_code, reason = "the kernel reads the links")]
pub struct Node {
    value: i32,
    child: Option<Box<Node>>,
    sibling: Option<Box<Node>>,
}

// The kernel, built from kernel/ into the file the build names.
uscope_views::uscope_kernel!(
    "tree",
    "tests/fixtures/rust/embedded-views/kernel/src/lib.rs",
    env!("USCOPE_TREE_KERNEL")
);

fn node(value: i32, child: Option<Node>, sibling: Option<Node>) -> Node {
    Node {
        value,
        child: child.map(Box::new),
        sibling: sibling.map(Box::new),
    }
}

#[inline(never)]
fn barrier(fixture: *const u8) {
    black_box(fixture);
}

fn main() {
    let tags = Tags {
        names: vec!["red", "green"],
    };
    let temperature = Celsius(21.5);
    let leaves = node(3, None, Some(node(4, None, None)));
    let branches = node(2, Some(leaves), Some(node(5, None, None)));
    let family = Tree {
        root: Some(Box::new(node(1, Some(branches), None))),
        count: 5,
    };
    let nobody = Tree {
        root: None,
        count: 0,
    };
    black_box((&tags, &temperature, &family, &nobody));
    barrier(std::ptr::from_ref(&tags).cast());
    let values = family.root.as_ref().map_or(0, |root| root.value);
    std::process::exit(i32::from(
        tags.names.len() != 2 || temperature.0 < 0.0 || values != 1 || nobody.count != 0,
    ));
}
