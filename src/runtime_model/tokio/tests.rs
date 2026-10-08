//! The tokio model on the `workers` fixture's real layout, with runtimes,
//! threads, and tasks the tests write: every state a task's bits can
//! hold, threads in each relation to a runtime, and damaged lists, which
//! no program shows on demand.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use proptest::prelude::*;

use super::layout::{Context, Flavor, Runtime};
use super::{CANCELLED, COMPLETE, NOTIFIED, POLL, RUNNING, TokioRuntime};
use crate::runtime_model::{
    ImageSymbol, Member, RuntimeImage, RuntimeModel, RuntimeStop, RuntimeTask, TaskRef,
    ThreadActivity,
};
use crate::{
    ImageAddress, IntegerValue, ModuleImage, TaskState, ThreadId, ThreadLocal, TypeInfo,
    TypeReference, VirtualAddress,
};

const FIXTURE: &str = "tokio-workers-o0";
/// Where the tests place what they write.
const HEAP: u64 = 0x7000_0000_0000;
/// Where each task's id and trailer are within its cell, as the tests'
/// vtables say.
const ID: u64 = 0x28;
const TRAILER: u64 = 0x40;
/// The bits of a task's state that are not its lifecycle, and one
/// reference.
const JOIN_INTEREST: u64 = 0b1000;
const JOIN_WAKER: u64 = 0b1_0000;
const REF_ONE: u64 = 1 << 6;

fn module() -> Arc<ModuleImage> {
    static MODULE: OnceLock<Arc<ModuleImage>> = OnceLock::new();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(FIXTURE);
    Arc::clone(MODULE.get_or_init(|| {
        crate::debug_info::load_module(
            &path,
            crate::ModuleImageId::new(0),
            &crate::debug_info::DebugFileSearch::default(),
        )
        .expect("run `just build-test-programs`")
        .image
    }))
}

/// The fixture's image, with some of tokio's names hidden, as a program
/// that lacks them would be, and tokio's sources where `source` says.
#[derive(Debug)]
struct Image {
    module: Arc<ModuleImage>,
    hidden: Vec<&'static str>,
    source: Option<&'static str>,
}

/// Where the release the model was verified against keeps its sources.
const VERIFIED_SOURCE: &str = "/crates/tokio-1.52.3/src/runtime/task/raw.rs";

impl RuntimeImage for Image {
    fn producers(&self) -> &[Arc<str>] {
        self.module.producers()
    }

    fn constant(&self, name: &str) -> Option<IntegerValue> {
        RuntimeImage::constant(self.module.as_ref(), name)
    }

    fn symbol(&self, name: &str) -> Option<ImageSymbol> {
        RuntimeImage::symbol(self.module.as_ref(), name)
    }

    fn function_answering(&self, name: &str) -> Option<ImageSymbol> {
        RuntimeImage::function_answering(self.module.as_ref(), name)
    }

    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>> {
        RuntimeImage::symbol_at(self.module.as_ref(), address)
    }

    fn has_function(&self, name: &str) -> bool {
        RuntimeImage::has_function(self.module.as_ref(), name)
    }

    fn function_body(&self, name: &str) -> Option<ImageAddress> {
        RuntimeImage::function_body(self.module.as_ref(), name)
    }

    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member> {
        RuntimeImage::member(self.module.as_ref(), type_name, path)
    }

    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>> {
        RuntimeImage::function_name(self.module.as_ref(), address)
    }

    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        RuntimeImage::thread_local(self.module.as_ref(), name)
    }

    fn thread_local_within(
        &self,
        scope: &str,
        name: &str,
    ) -> Option<Result<ThreadLocal, Arc<str>>> {
        RuntimeImage::thread_local_within(self.module.as_ref(), scope, name)
    }

    fn types_named(&self, name: &str) -> Vec<TypeReference> {
        if self.hidden.contains(&name) {
            return Vec::new();
        }
        RuntimeImage::types_named(self.module.as_ref(), name)
    }

    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        RuntimeImage::type_info(self.module.as_ref(), ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        RuntimeImage::same_type(self.module.as_ref(), left, right)
    }

    fn source_path_ending(&self, suffix: &str) -> Option<std::path::PathBuf> {
        self.source
            .filter(|source| source.ends_with(suffix))
            .map(std::path::PathBuf::from)
    }
}

/// Memory the tests write, and the threads they stop.
#[derive(Default)]
struct Memory {
    bytes: BTreeMap<u64, u8>,
    threads: BTreeMap<ThreadId, u64>,
}

impl Memory {
    fn write(&mut self, address: u64, value: u64, size: usize) {
        for (index, byte) in (0..).zip(&value.to_le_bytes()[..size]) {
            let at = address
                .checked_add(index)
                .expect("within the address space");
            self.bytes.insert(at, *byte);
        }
    }

    fn word(&mut self, address: u64, value: u64) {
        self.write(address, value, 8);
    }
}

impl RuntimeStop for Memory {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        for (index, byte) in (0..).zip(bytes.iter_mut()) {
            let Some(read) = address
                .get()
                .checked_add(index)
                .and_then(|at| self.bytes.get(&at))
            else {
                return false;
            };
            *byte = *read;
        }
        true
    }

    fn thread_pointer(&self, thread: ThreadId) -> Option<u64> {
        self.threads.get(&thread).copied()
    }

    fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
        None
    }

    fn load_bias(&self) -> u64 {
        0
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.threads.keys().copied().collect()
    }
}

/// A runtime the tests wrote: its `Arc`'s allocation, its handle, its
/// list's id and shards, and each shard's tasks' headers.
#[derive(Debug, Clone)]
struct Written {
    flavor: Flavor,
    arc: u64,
    handle: u64,
    list: u64,
    shards: u64,
    headers: Vec<Vec<u64>>,
}

/// The model, and the memory it reads.
struct World {
    model: TokioRuntime,
    memory: Memory,
    next: u64,
    /// A task vtable's poll function, and a worker's launch closure's.
    poll: u64,
    launch: u64,
}

impl World {
    fn new(hidden: &[&'static str]) -> Self {
        Self::with_source(hidden, Some(VERIFIED_SOURCE))
    }

    /// The model of an image whose tokio sources are at `source`.
    fn with_source(hidden: &[&'static str], source: Option<&'static str>) -> Self {
        let module = module();
        let polls = module
            .symbols()
            .iter()
            .filter_map(|symbol| {
                let name = crate::demangle::demangle(&symbol.name)?;
                name.starts_with(POLL).then(|| {
                    let launch = super::LAUNCH.iter().any(|launch| name.contains(launch));
                    (launch, symbol.address.get())
                })
            })
            .collect::<Vec<_>>();
        let find = |launch: bool| {
            polls
                .iter()
                .find(|(is_launch, _)| *is_launch == launch)
                .expect("a task's poll function")
                .1
        };
        let (poll, launch) = (find(false), find(true));
        let image = Image {
            module,
            hidden: hidden.to_vec(),
            source,
        };
        Self {
            model: TokioRuntime::bind(Arc::new(image)),
            memory: Memory::default(),
            next: HEAP,
            poll,
            launch,
        }
    }

    fn context(&self) -> &Context {
        self.model.layout.context.as_ref().expect("CONTEXT binds")
    }

    fn runtime_layout(&self, flavor: Flavor) -> &Runtime {
        self.context()
            .runtimes
            .iter()
            .find(|(known, ..)| *known == flavor)
            .map(|(.., runtime)| runtime)
            .expect("the flavor binds")
    }

    /// A fresh, 128-aligned allocation.
    fn allocate(&mut self, size: u64) -> u64 {
        let at = self.next;
        self.next += size.next_multiple_of(128) + 128;
        at
    }

    /// A vtable whose poll function is `poll`.
    fn vtable(&mut self, poll: u64) -> u64 {
        let tasks = self.model.layout.tasks.as_ref().expect("Header binds");
        let (poll_at, trailer_at, id_at) = (tasks.poll, tasks.trailer_offset, tasks.id_offset);
        let vtable = self.allocate(128);
        self.memory.word(vtable + poll_at, poll);
        self.memory.word(vtable + trailer_at, TRAILER);
        self.memory.word(vtable + id_at, ID);
        vtable
    }

    /// A task's cell, unlinked.
    fn cell(&mut self, vtable: u64, id: u64, state: u64, owner: u64) -> u64 {
        let tasks = self.model.layout.tasks.as_ref().expect("Header binds");
        let (state_at, vtable_at, owner_at) = (tasks.state, tasks.vtable, tasks.owner);
        let header = self.allocate(128);
        self.memory.word(header + state_at, state);
        self.memory.word(header + vtable_at, vtable);
        self.memory.word(header + owner_at, owner);
        self.memory.word(header + ID, id);
        header
    }

    /// Links a shard's tasks, in order, from its head.
    fn link(&mut self, shard: u64, headers: &[u64]) {
        let owned = &self.runtime_layout(Flavor::MultiThread).owned;
        let head_at = owned.head;
        let tasks = self.model.layout.tasks.as_ref().expect("Header binds");
        let (prev, next) = (tasks.prev, tasks.next);
        self.memory
            .word(shard + head_at, headers.first().copied().unwrap_or(0));
        for (index, header) in headers.iter().enumerate() {
            let before = index.checked_sub(1).map_or(0, |before| headers[before]);
            let after = headers.get(index + 1).copied().unwrap_or(0);
            self.memory.word(header + TRAILER + prev, before);
            self.memory.word(header + TRAILER + next, after);
        }
    }

    /// A runtime whose list's shards hold tasks of these ids and states.
    fn runtime(&mut self, flavor: Flavor, list: u64, shards: &[Vec<(u64, u64)>]) -> Written {
        let runtime = self.runtime_layout(flavor);
        let (data, owned_at) = (runtime.data, runtime.owned.at);
        let owned = &runtime.owned;
        let (id_at, shards_at, count_at, mask_at) =
            (owned.id, owned.shards, owned.count, owned.mask);
        let (shard_size, lock_at) = (owned.shard_size, owned.shard_lock);
        let arc = self.allocate(4096);
        let handle = arc + data;
        let at = handle + owned_at;
        let length = u64::try_from(shards.len()).expect("a few shards");
        let array = self.allocate(length * shard_size);
        self.memory.word(at + id_at, list);
        self.memory.word(at + shards_at, array);
        self.memory.word(at + shards_at + 8, length);
        let count = shards.iter().map(Vec::len).sum::<usize>();
        self.memory
            .word(at + count_at, u64::try_from(count).expect("a count"));
        self.memory.word(at + mask_at, length - 1);
        let vtable = self.vtable(self.poll);
        let mut headers = Vec::new();
        for (index, tasks) in (0..).zip(shards) {
            let shard = array + index * shard_size;
            self.memory.write(shard + lock_at, 0, 4);
            let cells = tasks
                .iter()
                .map(|&(id, state)| self.cell(vtable, id, state, list))
                .collect::<Vec<_>>();
            self.link(shard, &cells);
            headers.push(cells);
        }
        self.pool(flavor, handle, &[]);
        Written {
            flavor,
            arc,
            handle,
            list,
            shards: array,
            headers,
        }
    }

    /// The runtime's blocking pool, holding these tasks in a ring buffer
    /// of twice their number, the first of them at its end; where the
    /// pool's shared state is.
    fn pool(&mut self, flavor: Flavor, handle: u64, queued: &[(u64, bool)]) -> u64 {
        let spawner = self.runtime_layout(flavor).spawner;
        let pool = self.model.layout.pool.as_ref().expect("the pool binds");
        let (data, lock, head, len, buffer, capacity, size, task) = (
            pool.data,
            pool.lock,
            pool.head,
            pool.len,
            pool.buffer,
            pool.capacity,
            pool.task_size,
            pool.task,
        );
        let arc = self.allocate(1024);
        self.memory.word(handle + spawner, arc);
        let inner = arc + data;
        let room = u64::try_from(queued.len() * 2).expect("a few").max(1);
        let ring = self.allocate(room * size);
        let first = room - 1;
        self.memory.write(inner + lock, 0, 4);
        self.memory.word(inner + head, first);
        self.memory
            .word(inner + len, u64::try_from(queued.len()).expect("a few"));
        self.memory.word(inner + buffer, ring);
        self.memory.word(inner + capacity, room);
        for (index, &(id, launch)) in (0..).zip(queued) {
            let vtable = self.vtable(if launch { self.launch } else { self.poll });
            let cell = self.cell(vtable, id, NOTIFIED, 0);
            let slot = (first + index) % room;
            self.memory.word(ring + slot * size + task, cell);
        }
        inner
    }

    /// A thread whose `CONTEXT` names `runtime`, as a worker or not,
    /// polling `task`; a worker entered its runtime.
    fn thread(&mut self, tid: u64, runtime: Option<&Written>, worker: bool, task: Option<u64>) {
        self.entered_thread(tid, runtime, worker, worker, task);
    }

    /// A thread whose `CONTEXT` names `runtime`, as a worker or not, which
    /// entered it or not, polling `task`.
    fn entered_thread(
        &mut self,
        tid: u64,
        runtime: Option<&Written>,
        worker: bool,
        entered: bool,
        task: Option<u64>,
    ) {
        let context = self.context();
        let ThreadLocal::Offset(offset) = context.tls else {
            panic!("an executable's thread-local storage is at an offset");
        };
        let (state, alive) = (context.state, context.alive);
        let (handle_at, some_at, scheduler_at) = (
            context.handle.offset,
            context.handle_some,
            context.scheduler,
        );
        let (task_at, task_some) = (context.task.offset, context.task_some);
        let handle_tag =
            context
                .handle_option
                .tag_for(if runtime.is_some() { "Some" } else { "None" });
        let task_tag = context
            .task_option
            .tag_for(if task.is_some() { "Some" } else { "None" });
        let entered_at = context.entered.offset;
        let entered_tag =
            context
                .entered_state
                .tag_for(if entered { "Entered" } else { "NotEntered" });
        let flavor_tags = [Flavor::MultiThread, Flavor::CurrentThread].map(|flavor| {
            let name = self
                .context()
                .runtimes
                .iter()
                .find(|(known, ..)| *known == flavor)
                .map(|(_, name, _)| Arc::clone(name))
                .expect("the flavor binds");
            (flavor, self.context().flavors.tag_for(&name))
        });
        let pointer = HEAP + 0x1000_0000 * (tid + 1);
        let base = pointer.wrapping_add_signed(offset);
        self.memory.threads.insert(ThreadId::new(tid), pointer);
        self.memory.write(base + state, alive, 1);
        let (at, size, value) = handle_tag;
        self.memory.write(base + handle_at + at, value, size);
        if let Some(runtime) = runtime {
            let (_, (at, size, value)) = flavor_tags
                .iter()
                .find(|(flavor, _)| *flavor == runtime.flavor)
                .expect("a flavor");
            let handle = base + handle_at + some_at;
            self.memory.write(handle + at, *value, *size);
            let arc_at = self.runtime_layout(runtime.flavor).arc;
            self.memory.word(handle + arc_at, runtime.arc);
        }
        self.memory
            .word(base + scheduler_at, if worker { 0x1234_5000 } else { 0 });
        let (at, size, value) = entered_tag;
        self.memory.write(base + entered_at + at, value, size);
        let (at, size, value) = task_tag;
        self.memory.write(base + task_at + at, value, size);
        if let Some(task) = task {
            self.memory.word(base + task_at + task_some, task);
        }
    }

    /// Every task, `page` at a time, and the page's gaps.
    fn tasks(&self, page: usize, program_only: bool) -> (Vec<RuntimeTask>, Vec<String>) {
        let mut tasks = Vec::new();
        let mut gaps = Vec::new();
        let mut start = 0;
        for _ in 0..10_000 {
            let found = self.model.tasks(&self.memory, start, page, program_only);
            assert!(found.value.tasks.len() <= page);
            tasks.extend(found.value.tasks);
            gaps.extend(found.gaps.iter().map(ToString::to_string));
            match found.value.next {
                Some(next) => start = next,
                None => return (tasks, gaps),
            }
        }
        panic!("the pages never end");
    }

    fn activity(&self, tid: u64) -> ThreadActivity {
        self.model.thread_activity(&self.memory, ThreadId::new(tid))
    }
}

/// What the convention says a task's state bits mean, given whether a
/// worker polls it: a state, and its description.
fn convention(bits: u64, polled: bool) -> (TaskState, String) {
    let cancelled = if bits & CANCELLED == 0 {
        ""
    } else {
        ", cancelled"
    };
    match (
        bits & COMPLETE != 0,
        bits & RUNNING != 0,
        bits & NOTIFIED != 0,
        polled,
    ) {
        (true, ..) if cancelled.is_empty() => (TaskState::Exited, "completed".into()),
        (true, ..) => (TaskState::Exited, "cancelled".into()),
        (false, true, _, true) => (TaskState::Running, format!("running{cancelled}")),
        (false, true, _, false) => (
            TaskState::Unknown("a poll of the task begins or ends on some thread".into()),
            format!("running{cancelled}"),
        ),
        (false, false, true, _) => (TaskState::Runnable, format!("runnable{cancelled}")),
        (false, false, false, _) => (TaskState::Blocked, format!("suspended{cancelled}")),
    }
}

/// Every combination of the state's six flags, at three reference counts,
/// is one state, as the convention says; only a task some worker polls is
/// on a thread.
#[test]
fn each_state_is_read_as_tokio_defines_it() {
    let mut world = World::new(&[]);
    let mut tasks = Vec::new();
    for flags in 0..64_u64 {
        let bits = [
            RUNNING,
            COMPLETE,
            NOTIFIED,
            JOIN_INTEREST,
            JOIN_WAKER,
            CANCELLED,
        ]
        .into_iter()
        .enumerate()
        .filter(|(index, _)| flags & (1 << index) != 0)
        .fold(0, |bits, (_, bit)| bits | bit);
        for references in 1..=3 {
            tasks.push((flags * 4 + references, bits | (references * REF_ONE)));
        }
    }
    let runtime = world.runtime(Flavor::MultiThread, 7, &[tasks.clone()]);
    // A worker polls the running tasks with one reference.
    let polled = tasks
        .iter()
        .filter(|(id, bits)| bits & RUNNING != 0 && id % 4 == 1)
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    for (tid, id) in (100..).zip(&polled) {
        world.thread(tid, Some(&runtime), true, Some(*id));
    }
    let (listed, gaps) = world.tasks(4096, true);
    assert!(gaps.is_empty(), "{gaps:?}");
    assert_eq!(listed.len(), tasks.len());
    for (task, &(id, bits)) in listed.iter().zip(&tasks) {
        assert_eq!(task.number, id);
        let (state, detail) = convention(bits, polled.contains(&id));
        assert_eq!(
            (&task.state, task.detail.as_deref()),
            (&state, Some(&*detail)),
            "{bits:#b}"
        );
        assert_eq!(
            task.thread.is_some(),
            state == TaskState::Running,
            "{bits:#b}"
        );
        assert!(!task.internal);
    }
}

/// A worker runs the task it polls only when the task is its runtime's,
/// and is idle as it polls its own launch; a pool thread runs its
/// closure's task, and waits for one idle. A thread that never made its
/// context, names no runtime, or blocks on one is the program's own.
#[test]
fn a_thread_runs_the_task_its_context_names() {
    let mut world = World::new(&[]);
    let runtime = world.runtime(
        Flavor::MultiThread,
        3,
        &[vec![(10, RUNNING)], vec![(11, 0)]],
    );
    let current = world.runtime(Flavor::CurrentThread, 4, &[vec![(30, 0)]]);
    world.thread(1, Some(&runtime), true, Some(10));
    world.thread(2, Some(&runtime), true, Some(2));
    world.thread(3, Some(&runtime), false, Some(12));
    world.thread(4, Some(&runtime), false, None);
    world.thread(5, None, false, None);
    world.thread(6, Some(&runtime), true, Some(11));
    world.entered_thread(8, Some(&runtime), false, true, None);
    world.thread(9, Some(&current), true, None);
    world.thread(10, Some(&current), true, Some(30));
    // A thread that never touched its context.
    world.thread(7, Some(&runtime), true, Some(10));
    let state = world.context().state;
    let pointer = world.memory.threads[&ThreadId::new(7)];
    let ThreadLocal::Offset(offset) = world.context().tls else {
        panic!("an offset");
    };
    world
        .memory
        .write(pointer.wrapping_add_signed(offset) + state, 1, 1);

    let task = |number| ThreadActivity::Task {
        number,
        stack: crate::StackSegment::Task,
    };
    assert_eq!(world.activity(1), task(10));
    assert_eq!(world.activity(2), ThreadActivity::Idle);
    assert_eq!(world.activity(3), task(12));
    assert_eq!(world.activity(4), ThreadActivity::Idle);
    assert_eq!(world.activity(5), ThreadActivity::Outside);
    // A worker polling a suspended task is on it, though the task says
    // otherwise, as when a poll is about to begin.
    assert_eq!(world.activity(6), task(11));
    assert_eq!(world.activity(7), ThreadActivity::Outside);
    assert_eq!(world.activity(8), ThreadActivity::Outside);
    assert_eq!(world.activity(9), ThreadActivity::Outside);
    assert_eq!(world.activity(10), task(30));

    // The pool thread's task is listed as running there; its frames, and
    // the worker's task's, begin on their threads.
    let (listed, _) = world.tasks(4096, true);
    let blocking = listed
        .iter()
        .find(|task| task.number == 12)
        .expect("listed");
    assert_eq!(blocking.thread, Some(ThreadId::new(3)));
    for (number, thread) in [(10, 1), (12, 3)] {
        let context = world
            .model
            .task_context(
                &world.memory,
                TaskRef {
                    number,
                    locator: None,
                },
            )
            .expect("found")
            .expect("a task");
        assert!(matches!(context, super::TaskContext::OnThread(on) if on == ThreadId::new(thread)));
    }
    assert!(
        world
            .model
            .task_context(
                &world.memory,
                TaskRef {
                    number: 99,
                    locator: None
                }
            )
            .expect("read")
            .is_none()
    );
}

/// The blocking pool's queue is listed in its order, around the end of its
/// ring; a worker's launch is the runtime's own.
#[test]
fn the_blocking_pool_lists_its_queue_in_order() {
    let mut world = World::new(&[]);
    let runtime = world.runtime(Flavor::CurrentThread, 4, &[vec![(1, 0)]]);
    world.pool(
        runtime.flavor,
        runtime.handle,
        &[(20, true), (21, false), (22, false)],
    );
    world.thread(1, Some(&runtime), true, None);
    let (all, gaps) = world.tasks(2, false);
    assert!(gaps.is_empty(), "{gaps:?}");
    let numbers = all.iter().map(|task| task.number).collect::<Vec<_>>();
    assert_eq!(numbers, [1, 20, 21, 22]);
    assert!(all[1].internal && !all[2].internal);
    assert_eq!(
        all[2].detail.as_deref(),
        Some("queued in the blocking pool")
    );
    assert_eq!(all[2].state, TaskState::Runnable);
    let (program, _) = world.tasks(1, true);
    let numbers = program.iter().map(|task| task.number).collect::<Vec<_>>();
    assert_eq!(numbers, [1, 21, 22]);
}

/// Addresses read from the program's memory may be anything: a context
/// at the top of the address space, or a queue's head past its end, is
/// reported, never followed past the end of the address space.
#[test]
fn addresses_at_the_end_of_memory_are_reported_not_followed() {
    let mut world = World::new(&[]);
    let runtime = world.runtime(Flavor::MultiThread, 4, &[vec![(1, 0)]]);
    world.thread(1, Some(&runtime), true, None);
    let inner = world.pool(runtime.flavor, runtime.handle, &[(20, false), (21, false)]);
    let ThreadLocal::Offset(offset) = world.context().tls else {
        panic!("an executable's thread-local storage is at an offset");
    };
    let (state, alive) = (world.context().state, world.context().alive);
    for below in 0..512 {
        let base = u64::MAX - below;
        let tid = 1000 + below;
        world.memory.threads.insert(
            ThreadId::new(tid),
            base.wrapping_add_signed(offset.wrapping_neg()),
        );
        world.memory.write(base.wrapping_add(state), alive, 1);
        assert!(
            !matches!(world.activity(tid), ThreadActivity::Task { .. }),
            "{below}"
        );
        world.memory.threads.remove(&ThreadId::new(tid));
    }
    let head = world
        .model
        .layout
        .pool
        .as_ref()
        .expect("the pool binds")
        .head;
    world.memory.word(inner + head, u64::MAX);
    let (tasks, gaps) = world.tasks(64, false);
    assert!(tasks.iter().all(|task| task.number < 20), "{tasks:#?}");
    assert!(gaps.iter().any(|gap| gap.contains("queue")), "{gaps:?}");
}

/// A release other than the one verified is read wherever its layout
/// binds, and every page says it is unverified; sources whose path names
/// no release say the version is unknown. Neither is taken for granted.
#[test]
fn every_page_says_when_the_version_is_unverified_or_unknown() {
    let unknown = "tokio's version is unknown; its runtime is read as tokio 1.52's";
    for (source, gap) in [
        (
            Some("/crates/tokio-1.53.0/src/runtime/task/raw.rs"),
            "tokio 1.53.0 is unverified; its runtime is read as tokio 1.52's",
        ),
        (Some("/vendor/tokio/src/runtime/task/raw.rs"), unknown),
        (None, unknown),
    ] {
        let mut world = World::with_source(&[], source);
        let runtime = world.runtime(
            Flavor::MultiThread,
            4,
            &[vec![(2, 0), (4, 0)], vec![(1, 0)]],
        );
        world.thread(1, Some(&runtime), true, None);
        let (tasks, gaps) = world.tasks(1, true);
        let numbers = tasks.iter().map(|task| task.number).collect::<Vec<_>>();
        assert_eq!(numbers, [2, 4, 1], "{source:?}");
        assert_eq!(gaps, vec![gap.to_owned(); 3], "{source:?}");
    }
}

/// A name the program lacks makes only what reads it unavailable, and
/// says which.
#[test]
fn a_missing_name_makes_only_what_needs_it_unavailable() {
    let pool = "tokio::runtime::blocking::pool::Inner";
    let mut world = World::new(&[]);
    let runtime = world.runtime(Flavor::MultiThread, 3, &[vec![(10, 0)]]);
    world.thread(1, Some(&runtime), false, Some(12));
    let memory = world.memory;

    let without_pool = TokioRuntime::bind(Arc::new(Image {
        module: module(),
        hidden: vec![pool],
        source: Some(VERIFIED_SOURCE),
    }));
    let page = without_pool.tasks(&memory, 0, 64, true);
    let numbers = page
        .value
        .tasks
        .iter()
        .map(|task| task.number)
        .collect::<Vec<_>>();
    assert_eq!(numbers, [10, 12]);
    assert!(
        page.gaps.iter().any(|gap| gap.contains(pool)),
        "{:?}",
        page.gaps
    );

    let header = "tokio::runtime::task::core::Header";
    let without_header = TokioRuntime::bind(Arc::new(Image {
        module: module(),
        hidden: vec![header],
        source: Some(VERIFIED_SOURCE),
    }));
    let page = without_header.tasks(&memory, 0, 64, true);
    assert!(page.value.tasks.is_empty());
    assert!(
        page.gaps.iter().any(|gap| gap.contains(header)),
        "{:?}",
        page.gaps
    );
    // A pool thread's task is still known: it needs no task's header.
    assert!(matches!(
        without_header.thread_activity(&memory, ThreadId::new(1)),
        ThreadActivity::Task { number: 12, .. }
    ));

    let storage =
        "std::sys::thread_local::native::eager::Storage<tokio::runtime::context::Context>";
    let without_context = TokioRuntime::bind(Arc::new(Image {
        module: module(),
        hidden: vec![storage],
        source: Some(VERIFIED_SOURCE),
    }));
    let page = without_context.tasks(&memory, 0, 64, true);
    assert!(
        page.gaps.iter().any(|gap| gap.contains(storage)),
        "{:?}",
        page.gaps
    );
    assert!(matches!(
        without_context.thread_activity(&memory, ThreadId::new(1)),
        ThreadActivity::Unknown(reason) if reason.contains(storage)
    ));
}

/// How a test damages one task of a list.
#[derive(Debug, Clone, Copy)]
enum Damage {
    /// The task names another list as its owner.
    ForeignOwner,
    /// The task's back link names another task.
    BrokenBack,
    /// The shard's last task links to its head again.
    Cycle,
    /// The task's vtable polls no task.
    DataVtable,
    /// The shard's lock is held.
    HeldLock,
    /// The list's count is one more than it holds.
    WrongCount,
}

fn damage() -> impl Strategy<Value = Option<Damage>> {
    prop_oneof![
        Just(None),
        Just(Some(Damage::ForeignOwner)),
        Just(Some(Damage::BrokenBack)),
        Just(Some(Damage::Cycle)),
        Just(Some(Damage::DataVtable)),
        Just(Some(Damage::HeldLock)),
        Just(Some(Damage::WrongCount)),
    ]
}

fn shared_world() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// On a list of any shape, every task is listed once, whatever the page
    /// size; damage is reported with its shard, never listed past, and
    /// leaves every other shard listed.
    #[test]
    fn pages_list_every_task_once_and_report_damage(
        shard_bits in 0_u32..4,
        sizes in proptest::collection::vec(0_usize..5, 8),
        damage in damage(),
        target in any::<prop::sample::Index>(),
        page in 1_usize..12,
    ) {
        let _serial = shared_world().lock();
        let shards = 1_usize << shard_bits;
        // tokio keeps each task in the shard its id picks.
        let lists = (0..shards)
            .map(|shard| {
                (1..=sizes[shard % sizes.len()])
                    .map(|nth| (u64::try_from(nth * 64 + shard).expect("small"), 0))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let total = lists.iter().map(Vec::len).sum::<usize>();
        let mut world = World::new(&[]);
        let runtime = world.runtime(Flavor::MultiThread, 9, &lists);
        world.thread(1, Some(&runtime), false, None);

        // The damaged shard and task: the shard's index, and how many of
        // its tasks precede the damage.
        let nonempty = (0..shards).filter(|shard| !lists[*shard].is_empty()).collect::<Vec<_>>();
        let damaged = damage.filter(|_| !nonempty.is_empty()).map(|damage| {
            let shard = nonempty[target.index(nonempty.len())];
            let node = target.index(lists[shard].len());
            (damage, shard, node)
        });
        let tasks = world.model.layout.tasks.as_ref().expect("Header binds");
        let (owner, prev, next, vtable) = (tasks.owner, tasks.prev, tasks.next, tasks.vtable);
        let owned = &world.runtime_layout(Flavor::MultiThread).owned;
        let (shard_size, lock, count) = (owned.shard_size, owned.shard_lock, owned.count);
        let mut cut = None;
        if let Some((damage, shard, node)) = damaged {
            let header = runtime.headers[shard][node];
            let shard_at = runtime.shards + u64::try_from(shard).expect("small") * shard_size;
            match damage {
                Damage::ForeignOwner => {
                    world.memory.word(header + owner, runtime.list + 1);
                    cut = Some((shard, node));
                }
                Damage::BrokenBack => {
                    world.memory.word(header + TRAILER + prev, header + 1);
                    cut = Some((shard, node));
                }
                Damage::Cycle => {
                    let last = *runtime.headers[shard].last().expect("a task");
                    world.memory.word(last + TRAILER + next, runtime.headers[shard][0]);
                    cut = Some((shard, lists[shard].len()));
                }
                Damage::DataVtable => {
                    let data = world.vtable(world.poll + 1);
                    world.memory.word(header + vtable, data);
                    cut = Some((shard, node));
                }
                Damage::HeldLock => world.memory.write(shard_at + lock, 1, 4),
                Damage::WrongCount => {
                    let at = runtime.handle + world.runtime_layout(Flavor::MultiThread).owned.at;
                    world.memory.word(at + count, u64::try_from(total + 1).expect("small"));
                }
            }
        }

        let (listed, gaps) = world.tasks(page, true);
        let listed = listed.iter().map(|task| task.number).collect::<Vec<_>>();
        let expected = lists
            .iter()
            .enumerate()
            .flat_map(|(shard, tasks)| {
                let keep = match cut {
                    Some((damaged, node)) if damaged == shard => node,
                    _ => tasks.len(),
                };
                tasks[..keep].iter().map(|(id, _)| *id)
            })
            .collect::<Vec<_>>();
        prop_assert_eq!(&listed, &expected);
        let unique = listed.iter().collect::<BTreeSet<_>>();
        prop_assert_eq!(unique.len(), listed.len());
        let reported = gaps;
        match damaged {
            None => prop_assert!(reported.is_empty(), "{:?}", reported),
            Some((Damage::WrongCount, ..)) => {
                prop_assert!(reported.iter().any(|gap| gap.contains("counts")), "{:?}", reported);
            }
            Some((_, shard, _)) => prop_assert!(
                reported.iter().any(|gap| gap.contains(&format!("shard {shard} "))),
                "{:?}",
                reported
            ),
        }
    }
}
