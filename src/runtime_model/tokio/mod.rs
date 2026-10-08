//! tokio's runtime: tasks are the futures its schedulers poll, each in a
//! heap cell whose header begins with its state and vtable.
//!
//! No list names every runtime, so the model finds them through the
//! threads that entered one: each thread's `CONTEXT` holds the handle of
//! its runtime, its scheduler when it is a worker, and the task it polls.
//! A runtime's spawned tasks hang from its `OwnedTasks`, sharded linked
//! lists through each task's trailer; its blocking pool queues the
//! closures no thread runs yet. Every node is checked before it is read,
//! so a corrupted list is reported, never followed.

mod layout;
mod tasks;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use layout::{Context, Flavor, Layout, Owned, Tasks};

use super::records::{self, Missing};
use super::{
    Crossing, DynamicValue, Partial, RuntimeException, RuntimeHook, RuntimeImage, RuntimeModel,
    RuntimeSignals, RuntimeStop, RuntimeTask, StoredValue, TaskContext, TaskPage, TaskRef,
    ThreadActivity,
};
use crate::unwind::RegisterFile;
use crate::{ImageAddress, StackSegment, TaskState, ThreadId, ThreadLocal, VirtualAddress};

/// What tokio calls its tasks.
pub(super) const TASK_NOUN: (&str, &str) = ("task", "tasks");
/// The release the contract was checked against. Another is read the same
/// way wherever its debug information binds, and says it is unverified.
const VERIFIED: (u64, u64) = (1, 52);
/// A source file every build of tokio compiles, by which its version is
/// read from the directory Cargo unpacked it in.
const VERSIONED_SOURCE: &str = "src/runtime/task/raw.rs";
/// The task state's bits (`runtime/task/state.rs`), which tokio keeps in
/// constants its debug information does not describe.
const RUNNING: u64 = 0b1;
const COMPLETE: u64 = 0b10;
const NOTIFIED: u64 = 0b100;
const CANCELLED: u64 = 0b10_0000;
/// The most shards one list may have, and the most tasks one list or
/// queue is read for, so corrupted memory cannot make the debugger read
/// without end.
const MAX_SHARDS: u64 = 1 << 16;
const MAX_TASKS: u64 = 1 << 24;
/// The function each task's vtable polls it through.
const POLL: &str = "tokio::runtime::task::raw::poll::<";
/// The closure a multi-thread runtime's workers run in the blocking pool.
const LAUNCH: &str = "multi_thread::worker::Launch>::launch";

/// tokio, in an image that has its thread-local context.
pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Arc<str>>> {
    // A program stripped of its symbols shows no sign of tokio.
    let tls = image.thread_local_within("tokio::runtime::context::CONTEXT", "__RUST_STD_INTERNAL_VAL")?;
    if let Err(reason) = tls {
        return Some(Err(format!("tokio's context cannot be found: {reason}").into()));
    }
    Some(Ok(Arc::new(TokioRuntime::bind(Arc::clone(image)))))
}

/// The release a source path names, such as `tokio-1.52.3/src/…`.
fn version(path: &std::path::Path) -> Option<(u64, u64, Arc<str>)> {
    let mut components = path.components().rev();
    for _ in VERSIONED_SOURCE.split('/') {
        components.next()?;
    }
    let directory = components.next()?.as_os_str().to_str()?;
    let release = directory.strip_prefix("tokio-")?;
    let mut parts = release.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor, release.into()))
}

#[derive(Debug)]
struct TokioRuntime {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    layout: Layout,
    /// Why every result may be wrong: a release the contract was not
    /// checked against, or one the program does not name.
    caveat: Option<Arc<str>>,
    /// Whether each vtable, by image address, is a task's.
    vtables: Mutex<BTreeMap<u64, Option<Arc<str>>>>,
}

/// One runtime the stop's threads entered: its flavor, and its handle's
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instance {
    flavor: Flavor,
    handle: u64,
}

/// What one thread's `CONTEXT` says.
#[derive(Debug, Clone, Copy)]
struct ThreadContext {
    thread: ThreadId,
    runtime: Option<Instance>,
    /// Whether the thread runs a scheduler: a worker, or the thread a
    /// current-thread runtime blocks on.
    worker: bool,
    /// Whether the thread entered its runtime: a worker, or a thread that
    /// blocks on it.
    entered: bool,
    /// The task the thread polls.
    task: Option<u64>,
}

/// The runtimes at one stop and what each thread does for them.
#[derive(Debug, Default)]
struct Census {
    threads: Vec<ThreadContext>,
    runtimes: Vec<Instance>,
    gaps: Vec<Arc<str>>,
}

/// One runtime's task list, as read at a stop.
#[derive(Debug, Clone, Copy)]
struct List {
    id: u64,
    shards: u64,
    length: u64,
    count: u64,
}

/// Where a page of tasks begins: at the start, at a task of a runtime's
/// list, or at an index of the blocking pools' tasks. A task's header is
/// 128-aligned, which leaves the low bit for the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    Start,
    Node(u64),
    Blocking(u64),
}

impl Cursor {
    const fn decode(position: u64) -> Self {
        if position == 0 {
            Self::Start
        } else if position & 1 == 1 {
            Self::Blocking(position >> 1)
        } else {
            Self::Node(position)
        }
    }

    const fn encode(self) -> u64 {
        match self {
            Self::Start => 0,
            Self::Node(header) => header,
            Self::Blocking(index) => (index << 1) | 1,
        }
    }
}

impl TokioRuntime {
    fn bind(image: Arc<dyn RuntimeImage + Send + Sync>) -> Self {
        let caveat = match image.source_path_ending(VERSIONED_SOURCE).as_deref().map(version) {
            Some(Some((major, minor, _))) if (major, minor) == VERIFIED => None,
            Some(Some((.., release))) => Some(format!(
                "tokio {release} is unverified; its runtime is read as tokio {}.{}'s",
                VERIFIED.0, VERIFIED.1
            )),
            _ => Some(format!(
                "tokio's version is unknown; its runtime is read as tokio {}.{}'s",
                VERIFIED.0, VERIFIED.1
            )),
        };
        Self {
            layout: Layout::bind(image.as_ref()),
            image,
            caveat: caveat.map(Arc::from),
            vtables: Mutex::new(BTreeMap::new()),
        }
    }

    fn partial<T>(&self, value: T) -> Partial<T> {
        Partial {
            value,
            gaps: self.caveat.iter().cloned().collect(),
        }
    }

    fn context(&self) -> Result<&Context, Missing> {
        self.layout.context.as_ref().map_err(Arc::clone)
    }

    fn task_layout(&self) -> Result<&Tasks, Missing> {
        self.layout.tasks.as_ref().map_err(Arc::clone)
    }

    /// Every stopped thread's context, and the runtimes they entered in
    /// the order of their threads.
    fn census(&self, stop: &dyn RuntimeStop) -> Result<Census, Missing> {
        let context = self.context()?;
        let mut census = Census::default();
        for thread in stop.threads() {
            match Self::thread_context(stop, context, thread) {
                Ok(Some(found)) => {
                    if let Some(runtime) = found.runtime
                        && !census.runtimes.contains(&runtime)
                    {
                        census.runtimes.push(runtime);
                    }
                    census.threads.push(found);
                }
                Ok(None) => {}
                Err(reason) => census
                    .gaps
                    .push(format!("thread {thread}'s tokio context: {reason}").into()),
            }
        }
        Ok(census)
    }

    /// What a thread's `CONTEXT` says, or `None` when the thread never
    /// made it.
    fn thread_context(
        stop: &dyn RuntimeStop,
        context: &Context,
        thread: ThreadId,
    ) -> Result<Option<ThreadContext>, Arc<str>> {
        let base = thread_local(stop, &context.tls, thread)?;
        let unreadable = || Arc::<str>::from(format!("the context at {base:#x} is unreadable"));
        let state = records::read(stop, base + context.state, 1).ok_or_else(unreadable)?;
        if state != context.alive {
            return Ok(None);
        }
        let option = base + context.handle.offset;
        let runtime = if &*context.handle_option.active(stop, option)?.name == "Some" {
            let handle = option + context.handle_some;
            let flavor = context.flavors.active(stop, handle)?;
            let (flavor, _, layout) = context
                .runtimes
                .iter()
                .find(|(_, name, _)| *name == flavor.name)
                .ok_or_else(|| format!("the runtime's flavor {} is unknown", flavor.name))?;
            let arc = records::word(stop, handle + layout.arc).ok_or_else(unreadable)?;
            Some(Instance {
                flavor: *flavor,
                handle: arc.wrapping_add(layout.data),
            })
        } else {
            None
        };
        let worker = records::word(stop, base + context.scheduler).ok_or_else(unreadable)? != 0;
        let entered =
            &*context.entered_state.active(stop, base + context.entered.offset)?.name == "Entered";
        let option = base + context.task.offset;
        let task = if &*context.task_option.active(stop, option)?.name == "Some" {
            Some(records::word(stop, option + context.task_some).ok_or_else(unreadable)?)
        } else {
            None
        };
        Ok(Some(ThreadContext {
            thread,
            runtime,
            worker,
            entered,
            task,
        }))
    }

    fn owned(&self, flavor: Flavor) -> Result<&Owned, Missing> {
        self.context()?
            .runtimes
            .iter()
            .find(|(known, ..)| *known == flavor)
            .map(|(.., runtime)| &runtime.owned)
            .ok_or_else(|| format!("the {} runtime is unknown", flavor.describe()).into())
    }

    /// A runtime's task list.
    fn list(&self, stop: &dyn RuntimeStop, runtime: Instance) -> Result<List, Arc<str>> {
        let owned = self.owned(runtime.flavor)?;
        let at = runtime.handle + owned.at;
        let unreadable = || Arc::<str>::from(format!("the task list at {at:#x} is unreadable"));
        let word = |offset: u64| records::word(stop, at + offset).ok_or_else(unreadable);
        let list = List {
            id: word(owned.id)?,
            shards: word(owned.shards)?,
            length: word(owned.shards + 8)?,
            count: word(owned.count)?,
        };
        let mask = word(owned.mask)?;
        if !list.length.is_power_of_two()
            || list.length > MAX_SHARDS
            || mask != list.length - 1
            || list.count > MAX_TASKS
        {
            return Err(format!(
                "the task list at {at:#x} has {} shards, mask {mask:#x}, and {} tasks",
                list.length, list.count
            )
            .into());
        }
        Ok(list)
    }

    /// Whether the vtable at `vtable` is a task's: its poll function is
    /// tokio's `raw::poll`. The answer for each vtable is kept, as code
    /// does not change.
    fn checked_vtable(&self, stop: &dyn RuntimeStop, vtable: u64) -> Result<(), Arc<str>> {
        let tasks = self.task_layout()?;
        let poll = records::word(stop, vtable.wrapping_add(tasks.poll))
            .ok_or_else(|| format!("the vtable at {vtable:#x} is unreadable"))?;
        let image = poll.wrapping_sub(stop.load_bias());
        let verdict = self
            .vtables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(image)
            .or_insert_with(|| {
                let name = self.image.symbol_at(ImageAddress::new(image));
                match name {
                    Some(name) if name.starts_with(POLL) => None,
                    _ => Some(format!("the vtable at {vtable:#x} polls no task").into()),
                }
            })
            .clone();
        verdict.map_or(Ok(()), Err)
    }

    /// Whether a task polls the blocking closure a worker runs on.
    fn is_launch(&self, stop: &dyn RuntimeStop, vtable: u64) -> bool {
        let Ok(tasks) = self.task_layout() else {
            return false;
        };
        records::word(stop, vtable.wrapping_add(tasks.poll))
            .and_then(|poll| {
                self.image
                    .symbol_at(ImageAddress::new(poll.wrapping_sub(stop.load_bias())))
            })
            .is_some_and(|name| name.contains(LAUNCH))
    }
}

/// Where a thread's copy of a thread-local variable is.
fn thread_local(
    stop: &dyn RuntimeStop,
    tls: &ThreadLocal,
    thread: ThreadId,
) -> Result<u64, Arc<str>> {
    let pointer = stop
        .thread_pointer(thread)
        .ok_or("the thread's thread pointer is unreadable")?;
    let offset = match *tls {
        ThreadLocal::Offset(offset) => offset,
        ThreadLocal::Slot(slot) => {
            let slot = slot.get().wrapping_add(stop.load_bias());
            records::word(stop, slot)
                .map(u64::cast_signed)
                .ok_or("the slot of tokio's context is unreadable")?
        }
    };
    Ok(pointer.wrapping_add_signed(offset))
}

impl RuntimeModel for TokioRuntime {
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        program_only: bool,
    ) -> Partial<TaskPage> {
        let mut page = self.partial(TaskPage {
            tasks: Vec::new(),
            next: None,
        });
        match self.census(stop) {
            Ok(census) => self.page(stop, &census, Cursor::decode(start), limit, program_only, &mut page),
            Err(reason) => page
                .gaps
                .push(format!("tokio's tasks cannot be read: {reason}").into()),
        }
        page
    }

    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity {
        match self.activity(stop, thread) {
            Ok(activity) => activity,
            Err(reason) => ThreadActivity::Unknown(reason),
        }
    }

    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        let census = self.census(stop)?;
        let Some(found) = self.find(stop, &census, task)? else {
            return Ok(None);
        };
        match (found.state, found.thread) {
            (TaskState::Running, Some(thread)) => Ok(Some(TaskContext::OnThread(thread))),
            (TaskState::Running, None) => {
                Err(format!("the thread running task {} is unknown", task.number).into())
            }
            _ => Err(format!(
                "task {} is not running, and the debugger does not read a suspended \
                 task's frames",
                task.number
            )
            .into()),
        }
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
        &[]
    }

    fn exception(
        &self,
        _stop: &dyn RuntimeStop,
        _hook: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        Err("tokio reports no exception of its own".into())
    }

    fn dynamic_value(
        &self,
        _stop: &dyn RuntimeStop,
        _representation: &str,
        _value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>> {
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
        Err("tokio moves no task's stack".into())
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
        Err("tokio's tasks are not watched as they start".into())
    }

    fn task_noun(&self) -> &'static str {
        TASK_NOUN.0
    }
}

#[cfg(test)]
mod tests;
