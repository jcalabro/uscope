//! A runtime's task list, damaged by the program itself. Thirty-two
//! tasks park on a two-worker runtime, whose list has eight shards, a
//! task's id choosing its shard. The program stops at the checkpoint
//! `intact`, then damages the newest task of four shards and stops at the
//! checkpoint `corrupt`:
//!
//! - `owner`: its list's id is another list's;
//! - `vtable`: its vtable is data, which polls nothing;
//! - `broken`: its next task is an address nothing maps;
//! - `cycle`: its next task is itself.
//!
//! It prints `TRUTH damaged KIND ID SHARD` for each, and `TRUTH shard ID
//! SHARD` for every task. Then it exits, as tokio could not go on.
//!
//! A header is `repr(C)`: its state, the inject queue's link, the vtable,
//! and the list's id. Where the list's links are in a task's cell is
//! found from the tasks themselves: the newest task of a shard links to
//! the one bound before it.

use std::collections::BTreeMap;
use std::future::pending;

use tokio::task::JoinHandle;

const TASKS: usize = 32;
/// tokio's shards for two workers: four for each, rounded to a power of
/// two.
const SHARDS: u64 = 8;
const VTABLE: usize = 16;
const OWNER: usize = 24;
/// How far into a task's cell its links are looked for.
const CELL: usize = 1024;

/// A vtable's worth of data, which polls nothing.
static NOT_A_VTABLE: [u64; 16] = [0; 16];

async fn parked() {
    let me = truth::start();
    let _at = truth::at(me, "parked");
    pending::<()>().await; // AWAIT: parked
}

/// The address of a task's header, which its handle holds.
fn header(handle: &JoinHandle<()>) -> usize {
    assert_eq!(size_of::<JoinHandle<()>>(), size_of::<usize>());
    // SAFETY: a join handle is the pointer to its task's header.
    unsafe { std::mem::transmute_copy(handle) }
}

/// Where, in the cell at `from`, a word holds `target`.
fn offset_of_link(from: usize, target: usize) -> usize {
    (OWNER + 8..CELL)
        .step_by(8)
        .find(|offset| {
            // SAFETY: the cell is a live task's, larger than `CELL` bytes
            // of its own or of the heap block after it.
            unsafe { ((from + offset) as *const usize).read() == target }
        })
        .expect("a task links to its neighbor")
}

/// Writes one word into a task's cell.
fn damage(header: usize, offset: usize, value: usize) {
    // SAFETY: the task is parked for good, and no thread reads the word
    // until the program exits.
    unsafe { ((header + offset) as *mut usize).write_volatile(value) };
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    let handles = (0..TASKS)
        .map(|_| runtime.spawn(parked()))
        .collect::<Vec<_>>();
    while !(truth::all_parked(TASKS) && truth::workers_parked(runtime.handle())) {
        std::thread::yield_now();
    }
    // Each shard's tasks, newest first, as each was bound to the front.
    let mut shards = BTreeMap::<u64, Vec<(u64, usize)>>::new();
    for handle in &handles {
        let id = handle.id().to_string().parse::<u64>().expect("a number");
        truth::line(&[&"shard", &id, &(id % SHARDS)]);
        shards.entry(id % SHARDS).or_default().insert(0, (id, header(handle)));
    }
    truth::checkpoint("intact", Some(runtime.handle()));

    let (_, newest) = shards[&0][0];
    let (_, older) = shards[&0][1];
    let next = offset_of_link(newest, older);
    for (kind, shard) in [("owner", 0), ("vtable", 1), ("broken", 2), ("cycle", 3)] {
        let (id, header) = shards[&shard][0];
        match kind {
            "owner" => damage(header, OWNER, 0xdead),
            "vtable" => damage(header, VTABLE, NOT_A_VTABLE.as_ptr() as usize),
            "broken" => damage(header, next, 0x10),
            _ => damage(header, next, header),
        }
        truth::line(&[&"damaged", &kind, &id, &shard]);
    }
    truth::checkpoint("corrupt", None);
    std::process::exit(0);
}
