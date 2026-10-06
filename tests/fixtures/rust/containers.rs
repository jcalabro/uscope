//! Standard library containers, and deliberately corrupted ones, which the
//! built-in views present. Each `VIEW:` marker says what its expression
//! must show, evaluated in main() where barrier() is called: `{c*N}` stands
//! for N of the character c, and `problem:` says the view must refuse the
//! value, and why.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsString};
use std::fmt::Debug;
use std::hint::black_box;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

/// A sum type with a variant of named fields, which only the debugger reads.
#[expect(dead_code, reason = "the debugger reads it")]
enum Shape {
    Square { side: u32 },
    Circle(u32),
}

#[derive(Debug)]
#[expect(dead_code, reason = "the debugger reads it")]
struct Point {
    x: i32,
    y: i32,
}

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
    // Zero-sized elements, for which std stores no capacity.
    let units: Vec<()> = vec![(); 3]; // VIEW: units => len=3 [(), (), ()]
    let mut unit_ring: VecDeque<()> = VecDeque::new(); // VIEW: unit_ring => len=2 [(), ()]
    unit_ring.push_back(());
    unit_ring.push_front(());
    let mut hashed: HashMap<i32, i32> = HashMap::new(); // VIEW: hashed => len=2 {1: 10, 2: 20} (any order)
    hashed.insert(1, 10);
    hashed.insert(2, 20);
    let mut named: HashMap<String, u64> = HashMap::new(); // VIEW: named => len=1 {"one": 1}
    named.insert(String::from("one"), 1);
    let no_hashed: HashMap<i32, i32> = HashMap::new(); // VIEW: no_hashed => len=0 {}
    let many_hashed: HashMap<u32, u32> = (0..300).map(|key| (key, key * 2)).collect(); // VIEW: many_hashed => count: 300
    let set: HashSet<u8> = HashSet::from([7, 9]); // VIEW: set => len=2 [7, 9] (any order)
    let no_set: HashSet<u8> = HashSet::new(); // VIEW: no_set => len=0 []
    // A pointer to a string in no mapped memory, and one to none at all.
    let lost_text = 0x10 as *const String; // VIEW: lost_text => problem: inaccessible
    let no_text: *const String = std::ptr::null(); // VIEW: no_text => stored

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

    // Pointers, cells, sums, and trait objects.
    let boxed: Box<i32> = Box::new(42); // VIEW: boxed => 42
    let rc = Rc::new(7_u64); // VIEW: rc => 7
    let rc_too = Rc::clone(&rc); // VIEW: rc_too => children: strong = 2, weak = 1, [raw]
    let rc_weak = Rc::downgrade(&rc); // VIEW: rc_weak => 7
    let no_weak: Weak<u64> = Weak::new(); // VIEW: no_weak => dangling
    let dropped = Rc::downgrade(&Rc::new(1_u8)); // VIEW: dropped => dropped
    let arc = Arc::new(String::from("shared")); // VIEW: arc => "shared"
    let arc_weak = Arc::downgrade(&arc); // VIEW: arc_weak => children: capacity = 6, strong = 1, weak = 1, [raw]
    let cell = Cell::new(5_i32); // VIEW: cell => 5
    let ref_cell = RefCell::new(vec![1, 2]); // VIEW: ref_cell => children: [0] = 1, [1] = 2, borrow = 1, [raw]
    let borrowed = ref_cell.borrow();
    let mutex = Mutex::new(9_u8); // VIEW: mutex => children: locked = false, poisoned = false, [raw]
    let some: Option<i32> = Some(4); // VIEW: some => Some(4)
    let nothing: Option<i32> = None; // VIEW: nothing => None
    let ok: Result<u32, String> = Ok(7); // VIEW: ok => Ok(7)
    let failed: Result<u32, String> = Err(String::from("no")); // VIEW: failed => Err("no")
    let shape = Shape::Square { side: 4 }; // VIEW: shape => Square {side: 4}
    let dynamic: Box<dyn Debug> = Box::new(Point { x: 1, y: 2 }); // VIEW: dynamic => Point {x: 1, y: 2}

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
        &units,
        &unit_ring,
        &hashed,
        &named,
        &no_hashed,
        &many_hashed,
        &set,
        &no_set,
        &lost_text,
        &no_text,
        &past_capacity,
        &dangling,
        &boxed,
        &rc,
        &rc_too,
        &rc_weak,
        &no_weak,
        &dropped,
        &arc,
        &arc_weak,
        &cell,
        &ref_cell,
        &borrowed,
        &mutex,
        &some,
        &nothing,
        &ok,
        &failed,
        &shape,
        &dynamic,
    ));
    barrier(std::ptr::from_ref(&text).cast());
    std::process::exit(i32::from(ints.len() + many.len() != 303));
}
