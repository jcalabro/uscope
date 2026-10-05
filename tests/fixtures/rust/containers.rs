//! Standard library containers, and deliberately corrupted ones, which the
//! built-in views present. Each `VIEW:` marker says what its expression
//! must show, evaluated in main() where barrier() is called: `{c*N}` stands
//! for N of the character c, and `problem:` says the view must refuse the
//! value, and why.

use std::collections::VecDeque;
use std::ffi::{CString, OsString};
use std::hint::black_box;
use std::mem::ManuallyDrop;
use std::path::PathBuf;

#[inline(never)]
fn barrier(fixture: *const u8) {
    black_box(fixture);
}

fn main() {
    let text = String::from("hello, world"); // VIEW: text => "hello, world"
    let empty_text = String::new(); // VIEW: empty_text => ""
    let long_text = "y".repeat(300); // VIEW: long_text => "{y*256}"... (300 bytes)
    let boxed_text: Box<str> = Box::from("boxed"); // VIEW: boxed_text => "boxed"
    let path = PathBuf::from("/tmp/uscope"); // VIEW: path => "/tmp/uscope"
    let os_text = OsString::from("os text"); // VIEW: os_text => "os text"
    let c_text = CString::new("c text").expect("no NUL"); // VIEW: c_text => "c text"
    let ints: Vec<i32> = vec![1, 2, 3]; // VIEW: ints => len=3 [1, 2, 3]
    let no_ints: Vec<u64> = Vec::new(); // VIEW: no_ints => len=0 []
    let words = vec![String::from("one"), String::from("two")]; // VIEW: words => len=2 ["one", "two"]
    let many: Vec<u32> = (0..300).collect(); // VIEW: many => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]
    // Pushed at both ends, so its elements wrap around its buffer.
    let mut ring: VecDeque<i32> = VecDeque::with_capacity(4); // VIEW: ring => len=4 [1, 2, 3, 4]
    ring.push_back(3);
    ring.push_back(4);
    ring.push_front(2);
    ring.push_front(1);

    // A vector longer than its capacity: its length is the word that holds
    // 4 when its capacity is 8, whatever order Rust gives its fields.
    let mut four: Vec<i32> = Vec::with_capacity(8);
    four.extend([10, 11, 12, 13]);
    // SAFETY: a Vec<i32> is three words, and the result is never read or
    // dropped; the debugger reads it.
    let mut fields: [usize; 3] = unsafe { std::mem::transmute(ManuallyDrop::new(four)) };
    let length = fields.iter().position(|field| *field == 4).expect("a length field");
    fields[length] = 9;
    // SAFETY: as above.
    let past_capacity: ManuallyDrop<Vec<i32>> = unsafe { std::mem::transmute(fields) }; // VIEW: past_capacity.value.0 => problem: check
    // A vector whose elements are in no mapped memory.
    // SAFETY: the vector is never read or dropped; the debugger reads it.
    let dangling = ManuallyDrop::new(unsafe { Vec::from_raw_parts(0x10 as *mut i32, 2, 2) }); // VIEW: dangling.value.0 => len=2 [<unavailable>, …]

    black_box((
        &text,
        &empty_text,
        &long_text,
        &boxed_text,
        &path,
        &os_text,
        &c_text,
        &ints,
        &no_ints,
        &words,
        &many,
        &ring,
        &past_capacity,
        &dangling,
    ));
    barrier(std::ptr::from_ref(&text).cast());
    std::process::exit(i32::from(ints.len() + many.len() != 303));
}
