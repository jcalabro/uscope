//! What a fixture reports about itself, for tests/tokio to compare the
//! debugger against.
//!
//! Each fixture task calls [`start`] for its own id, which it keeps in a
//! local named `me` saved across its awaits, and [`end`] when its body
//! ends. Before an await a test cares about, it holds the guard [`at`]
//! returns for as long as the await lasts; the await's line carries a
//! `// AWAIT: tag` marker. The nesting of guards is the task's logical
//! stack. At a checkpoint the program waits until nothing moves, prints
//! `TRUTH` lines, and calls [`truth_reached`], where a test stops.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Write;
use std::sync::Mutex;

/// What one task reported.
#[derive(Default)]
struct Task {
    /// Await tags held, outermost first.
    awaits: Vec<&'static str>,
    /// Values the task recorded, by name.
    values: BTreeMap<&'static str, String>,
    /// The thread the task last reported from.
    thread: i32,
    /// Executions of each counted line.
    hits: BTreeMap<&'static str, u64>,
}

static TASKS: Mutex<BTreeMap<u64, Task>> = Mutex::new(BTreeMap::new());

fn with_task<T>(id: u64, update: impl FnOnce(&mut Task) -> T) -> T {
    let mut tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    update(tasks.entry(id).or_default())
}

/// The current task's id, as tokio numbers it.
pub fn id() -> u64 {
    tokio::task::id().to_string().parse().expect("a task id is a number")
}

/// The calling thread's id.
pub fn gettid() -> i32 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::gettid() }
}

/// Registers the current task as alive and returns its id.
pub fn start() -> u64 {
    let me = id();
    with_task(me, |task| task.thread = gettid());
    me
}

/// Removes a task whose body has ended.
pub fn end(me: u64) {
    TASKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&me);
}

/// Records that the task is on this thread now.
pub fn here(me: u64) {
    with_task(me, |task| task.thread = gettid());
}

/// Records a value the task's checkpoint names.
pub fn value(me: u64, name: &'static str, value: impl Display) {
    let text = value.to_string();
    with_task(me, |task| task.values.insert(name, text));
}

/// Counts one execution of a line.
pub fn hit(me: u64, tag: &'static str) {
    with_task(me, |task| *task.hits.entry(tag).or_default() += 1);
}

/// A task's await, held for as long as the await lasts.
pub struct At {
    me: u64,
}

impl Drop for At {
    fn drop(&mut self) {
        with_task(self.me, |task| {
            task.awaits.pop();
            task.thread = gettid();
        });
    }
}

/// Marks the await the task is about to make.
#[must_use]
pub fn at(me: u64, tag: &'static str) -> At {
    with_task(me, |task| task.awaits.push(tag));
    At { me }
}

/// Prints one `TRUTH` line.
pub fn line(fields: &[&dyn Display]) {
    let mut text = String::from("TRUTH");
    for field in fields {
        text.push('\t');
        text.push_str(&field.to_string());
    }
    text.push('\n');
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes()).expect("print a truth line");
    out.flush().expect("flush a truth line");
}

/// The number of tasks that registered an await `tag`.
pub fn parked_at(tag: &str) -> usize {
    TASKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter(|task| task.awaits.last() == Some(&tag))
        .count()
}

/// Whether every task registered has an await registered.
pub fn all_parked(expected: usize) -> bool {
    let tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    tasks.len() == expected && tasks.values().all(|task| !task.awaits.is_empty())
}

/// Whether every worker of the runtime is parked: an odd park count
/// means a worker is parked, tokio says.
pub fn workers_parked(handle: &tokio::runtime::Handle) -> bool {
    let metrics = handle.metrics();
    (0..metrics.num_workers()).all(|worker| metrics.worker_park_unpark_count(worker) % 2 == 1)
}

/// Prints a checkpoint's lines and stops there under a debugger. The
/// caller has waited until nothing moves.
pub fn checkpoint(name: &str, handle: Option<&tokio::runtime::Handle>) {
    line(&[&"checkpoint", &name]);
    if let Some(handle) = handle {
        let metrics = handle.metrics();
        line(&[&"alive", &metrics.num_alive_tasks()]);
        line(&[&"workers", &metrics.num_workers()]);
    }
    {
        let tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (id, task) in tasks.iter() {
            let awaits = task.awaits.iter().rev().copied().collect::<Vec<_>>().join(",");
            line(&[&"task", id, &awaits, &task.thread]);
            for (name, value) in &task.values {
                line(&[&"value", id, name, value]);
            }
            for (tag, count) in &task.hits {
                line(&[&"hit", id, tag, count]);
            }
        }
    }
    line(&[&"main", &gettid()]);
    truth_reached();
}

/// Where a debugger stops at each checkpoint.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn truth_reached() {
    std::hint::black_box(());
}

/// Where a debugger stops in a task, which passes its own id.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn task_reached(me: u64) {
    std::hint::black_box(me);
}
