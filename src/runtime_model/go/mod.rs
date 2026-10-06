//! Go's runtime: goroutines are its tasks, listed in `runtime.allgs`, and a
//! thread's goroutine is the g in its thread-local storage.
//!
//! The model checks every g it follows against `allgs`, so a corrupted
//! pointer is reported, never read as a goroutine.

mod layout;

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use layout::{Goroutines, Layout, Missing, Threads};

use super::{
    CodeAddress, Partial, RuntimeImage, RuntimeModel, RuntimeStop, RuntimeTask, TaskContext,
    TaskPage, ThreadActivity,
};
use crate::unwind::RegisterFile;
use crate::{AddressRange, ImageAddress, TaskStack, TaskState, ThreadId, VirtualAddress};

/// The release the contract was checked against. Another release is read
/// the same way wherever its debug information binds, and says it is
/// unverified.
const VERIFIED: (u64, u64) = (1, 27);
/// x86-64's DWARF register numbers for the registers a parked goroutine
/// saves.
const RBP: u16 = 6;
const RSP: u16 = 7;
const RIP: u16 = 16;
/// The first release whose runtime the model can read at all.
const OLDEST: (u64, u64) = (1, 20);
/// The most goroutines one list reads, so a corrupted `allglen` cannot
/// make the debugger read without end.
const MAX_GOROUTINES: u64 = 1 << 24;

/// The Go runtime in an image whose units Go compiled.
pub fn detect(
    image: Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Arc<str>>> {
    let version = image
        .producers()
        .iter()
        .find_map(|producer| version(producer))?;
    Some(GoRuntime::bind(image, version).map(|model| Arc::new(model) as Arc<dyn RuntimeModel>))
}

/// The release in a Go producer, such as `Go cmd/compile go1.27.1; regabi`.
fn version(producer: &str) -> Option<(u64, u64, Arc<str>)> {
    let release = producer.strip_prefix("Go cmd/compile go")?;
    let release = release.split([';', ' ']).next()?;
    let mut parts = release.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, format!("go{release}").into()))
}

#[derive(Debug)]
struct GoRuntime {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    layout: Layout,
    /// Why every result may be wrong: a release the contract was not
    /// checked against.
    unverified: Option<Arc<str>>,
    /// Where a new thread runs before its thread-local g is set.
    starting: Vec<AddressRange<ImageAddress>>,
    /// The functions whose goroutines are the runtime's own work, with
    /// those that are the program's despite being in the runtime.
    internal: Internal,
    /// The runtime's names for goroutine statuses and wait reasons, which
    /// are static data read at the first stop that needs them.
    names: OnceLock<Names>,
}

impl GoRuntime {
    fn bind(
        image: Arc<dyn RuntimeImage + Send + Sync>,
        (major, minor, release): (u64, u64, Arc<str>),
    ) -> Result<Self, Arc<str>> {
        if (major, minor) < OLDEST {
            return Err(format!("{release}'s runtime is too old to read").into());
        }
        let unverified = ((major, minor) != VERIFIED).then(|| {
            format!(
                "{release} is unverified; its runtime is read as go{}.{}'s",
                VERIFIED.0, VERIFIED.1
            )
            .into()
        });
        let starting = ["runtime.clone", "runtime.settls"]
            .into_iter()
            .filter_map(|name| {
                let symbol = image.symbol(name)?;
                Some(AddressRange {
                    start: symbol.address,
                    end: ImageAddress::new(symbol.address.get().checked_add(symbol.size?)?),
                })
            })
            .collect();
        Ok(Self {
            layout: Layout::bind(image.as_ref()),
            internal: Internal::bind(image.as_ref()),
            image,
            unverified,
            starting,
            names: OnceLock::new(),
        })
    }

    fn partial<T>(&self, value: T) -> Partial<T> {
        Partial {
            value,
            gaps: self.unverified.iter().cloned().collect(),
        }
    }

    fn goroutines(&self) -> Result<&Goroutines, Missing> {
        self.layout.goroutines.as_ref().map_err(Arc::clone)
    }

    fn threads(&self) -> Result<&Threads, Missing> {
        self.layout.threads.as_ref().map_err(Arc::clone)
    }

    /// The g pointers in `allgs`, as many as `allglen` publishes.
    fn allgs(&self, stop: &dyn RuntimeStop) -> Result<Vec<u64>, Arc<str>> {
        let layout = self.goroutines()?;
        let bias = stop.load_bias();
        let at = |address: ImageAddress| VirtualAddress::new(address.get().wrapping_add(bias));
        let length = word(stop, at(layout.allglen)).ok_or("runtime.allglen is unreadable")?;
        let array = word(stop, at(layout.allgs)).ok_or("runtime.allgs is unreadable")?;
        let capacity = word(
            stop,
            VirtualAddress::new(at(layout.allgs).get().wrapping_add(8)),
        )
        .ok_or("runtime.allgs is unreadable")?;
        if length > capacity || length > MAX_GOROUTINES {
            return Err(
                format!("runtime.allglen is {length}, beyond what runtime.allgs holds").into(),
            );
        }
        let mut bytes = vec![0; usize::try_from(length * 8).map_err(|_| "allgs is too large")?];
        if !stop.read(VirtualAddress::new(array), &mut bytes) {
            return Err(format!("runtime.allgs at {array:#x} is unreadable").into());
        }
        Ok(bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|chunk| u64::from_le_bytes(*chunk))
            .collect())
    }

    /// Reads one goroutine, or `None` for one that is dead.
    fn goroutine(
        &self,
        stop: &dyn RuntimeStop,
        names: &Names,
        g: u64,
    ) -> Result<Option<RuntimeTask>, Arc<str>> {
        let layout = self.goroutines()?;
        let unreadable = || Arc::<str>::from(format!("goroutine at {g:#x} is unreadable"));
        let field = |offset: u64, size: usize| {
            read_unsigned(stop, VirtualAddress::new(g.wrapping_add(offset)), size)
                .ok_or_else(unreadable)
        };
        let word = |offset| field(offset, 8);
        let status = field(layout.status, 4)?;
        let statuses = &layout.statuses;
        let base = status & !statuses.scan;
        if base == statuses.dead || Some(base) == statuses.dead_extra {
            return Ok(None);
        }
        let number = word(layout.goid)?;
        // A goroutine is on a thread while its m runs it, or runs the
        // runtime's code for it, as when it is being parked or scheduled.
        let m = word(layout.m)?;
        let thread = if m == 0 {
            None
        } else {
            let m_word = |offset: u64| {
                read_unsigned(stop, VirtualAddress::new(m.wrapping_add(offset)), 8).ok_or_else(
                    || Arc::<str>::from(format!("the thread of goroutine {number} is unreadable")),
                )
            };
            let procid = m_word(layout.m_procid)?;
            (procid != 0 && m_word(layout.m_curg)? == g).then(|| ThreadId::new(procid))
        };
        let state = if base == statuses.running || base == statuses.syscall {
            TaskState::Running
        } else if base == statuses.runnable || base == statuses.preempted {
            TaskState::Runnable
        } else if base == statuses.waiting
            || base == statuses.copystack
            || Some(base) == statuses.leaked
        {
            TaskState::Blocked
        } else if base == statuses.idle {
            TaskState::Unknown("the goroutine is being created".into())
        } else {
            TaskState::Unknown(format!("the goroutine's status {status:#x} is unknown").into())
        };
        // A waiting goroutine is described by why it waits, as the
        // runtime's own tracebacks do; any other by its status.
        let wait_reason = field(layout.wait_reason, 1)?;
        let detail = if base == statuses.waiting && wait_reason != 0 {
            names
                .wait_reasons
                .get(usize::try_from(wait_reason).unwrap_or(usize::MAX))
                .cloned()
                .flatten()
        } else {
            names
                .statuses
                .get(usize::try_from(base).unwrap_or(usize::MAX))
                .cloned()
                .flatten()
        };
        let address = |value: u64| (value != 0).then(|| VirtualAddress::new(value));
        let on_thread = base == statuses.running || base == statuses.syscall;
        let entry = word(layout.startpc)?;
        // A parked goroutine resumes after the call that saved its
        // registers, except one that never ran, which begins at its entry.
        let resume = word(layout.sched_pc)?;
        let resume = address(resume).map(|address| CodeAddress {
            address,
            after_call: resume != entry,
        });
        // A goroutine is created by a call to the runtime.
        let creation = address(word(layout.gopc)?).map(|address| CodeAddress {
            address,
            after_call: true,
        });
        Ok(Some(RuntimeTask {
            number,
            state,
            detail,
            thread,
            resume: if on_thread { None } else { resume },
            creation,
            entry: address(entry),
            parent: layout
                .parent_goid
                .map(word)
                .transpose()?
                .filter(|parent| *parent != 0),
            internal: self
                .internal
                .is_internal(self.image.as_ref(), entry.wrapping_sub(stop.load_bias())),
        }))
    }

    fn names(&self, stop: &dyn RuntimeStop) -> &Names {
        self.names.get_or_init(|| Names::read(self, stop))
    }
}

impl RuntimeModel for GoRuntime {
    fn tasks(&self, stop: &dyn RuntimeStop, start: u64, limit: usize) -> Partial<TaskPage> {
        let mut page = self.partial(TaskPage {
            tasks: Vec::new(),
            next: None,
        });
        let allgs = match self.allgs(stop) {
            Ok(allgs) => allgs,
            Err(reason) => {
                page.gaps.push(reason);
                return page;
            }
        };
        let names = self.names(stop);
        let mut index = start;
        while let Some(&g) = usize::try_from(index)
            .ok()
            .and_then(|index| allgs.get(index))
        {
            if page.value.tasks.len() == limit {
                page.value.next = Some(index);
                break;
            }
            index += 1;
            match self.goroutine(stop, names, g) {
                Ok(Some(task)) => page.value.tasks.push(task),
                Ok(None) => {}
                Err(reason) => page.gaps.push(reason),
            }
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
        number: u64,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        let layout = self.goroutines()?;
        let Some((g, task)) = self.find(stop, number)? else {
            return Ok(None);
        };
        // A goroutine that runs, or makes a system call, has its registers
        // on its thread; any other saved them when it last stopped running.
        if task.state == TaskState::Running {
            return task
                .thread
                .map(|thread| Some(TaskContext::OnThread(thread)))
                .ok_or_else(|| format!("the thread running goroutine {number} is unknown").into());
        }
        let word = |offset: u64| {
            read_unsigned(stop, VirtualAddress::new(g.wrapping_add(offset)), 8).ok_or_else(|| {
                Arc::<str>::from(format!(
                    "goroutine {number}'s saved registers are unreadable"
                ))
            })
        };
        let (pc, sp, bp) = (
            word(layout.sched_pc)?,
            word(layout.sched_sp)?,
            word(layout.sched_bp)?,
        );
        if pc == 0 || sp == 0 {
            return Err(format!("goroutine {number} has no saved registers").into());
        }
        let mut registers = RegisterFile::new([(RIP, pc), (RSP, sp)]);
        // A saved frame pointer of zero is unknown, not a frame at zero.
        if bp != 0 {
            registers.set(RBP, bp);
        }
        Ok(Some(TaskContext::Saved {
            registers,
            after_call: task.resume.is_none_or(|resume| resume.after_call),
        }))
    }
}

impl GoRuntime {
    /// The goroutine numbered `number`: its g, and what it is.
    fn find(
        &self,
        stop: &dyn RuntimeStop,
        number: u64,
    ) -> Result<Option<(u64, RuntimeTask)>, Arc<str>> {
        let names = self.names(stop);
        // An unreadable goroutine may be the one sought, so it is absent
        // only once every goroutine was read.
        let mut unreadable = None;
        for g in self.allgs(stop)? {
            match self.goroutine(stop, names, g) {
                Ok(Some(task)) if task.number == number => return Ok(Some((g, task))),
                Ok(_) => {}
                Err(reason) => unreadable = unreadable.or(Some(reason)),
            }
        }
        unreadable.map_or(Ok(None), |reason| {
            Err(format!("goroutine {number} was not found, and {reason}").into())
        })
    }

    fn activity(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<ThreadActivity, Arc<str>> {
        let threads = self.threads()?;
        let layout = self.goroutines()?;
        let pointer = stop
            .thread_pointer(thread)
            .ok_or("the thread's thread pointer is unreadable")?;
        if let Some(instruction) = stop.instruction(thread) {
            let image = ImageAddress::new(instruction.get().wrapping_sub(stop.load_bias()));
            if self.starting.iter().any(|range| range.contains(image)) {
                return Err("the thread is starting, before its goroutine is set".into());
            }
        }
        let slot = VirtualAddress::new(pointer.wrapping_add_signed(threads.tls_g));
        let g = word(stop, slot).ok_or("the thread's goroutine is unreadable")?;
        if g == 0 {
            return Ok(ThreadActivity::Idle);
        }
        let read = |address: u64| {
            word(stop, VirtualAddress::new(address)).ok_or_else(|| {
                Arc::<str>::from(format!("the thread's goroutine at {g:#x} is unreadable"))
            })
        };
        let m = read(g.wrapping_add(threads.g_m))?;
        if m == 0 {
            return Err(format!("the thread's goroutine at {g:#x} has no m").into());
        }
        let (g0, gsignal, curg) = (
            read(m.wrapping_add(threads.m_g0))?,
            read(m.wrapping_add(threads.m_gsignal))?,
            read(m.wrapping_add(threads.m_curg))?,
        );
        let (task, stack) = if g == g0 {
            (curg, TaskStack::System)
        } else if g == gsignal {
            (curg, TaskStack::Signal)
        } else {
            (g, TaskStack::Own)
        };
        if task == 0 {
            return Ok(ThreadActivity::Idle);
        }
        // Only a g the runtime lists is followed.
        let allgs = self.allgs(stop)?.into_iter().collect::<HashSet<_>>();
        if !allgs.contains(&task) {
            return Err(
                format!("the thread's goroutine at {task:#x} is not in runtime.allgs").into(),
            );
        }
        let number = read(task.wrapping_add(layout.goid))?;
        Ok(ThreadActivity::Task { number, stack })
    }
}

/// Which goroutines are the runtime's own work, as the runtime itself
/// decides: those that began in a runtime function, except the program's
/// main goroutine and a few that run the program's code.
#[derive(Debug, Default)]
struct Internal {
    /// Functions in the runtime whose goroutines run the program's code.
    program: Vec<ImageAddress>,
}

impl Internal {
    fn bind(image: &dyn RuntimeImage) -> Self {
        Self {
            program: [
                "runtime.main",
                "runtime.corostart",
                "runtime.handleAsyncEvent",
            ]
            .into_iter()
            .filter_map(|name| image.symbol(name).map(|symbol| symbol.address))
            .collect(),
        }
    }

    fn is_internal(&self, image: &dyn RuntimeImage, entry: u64) -> bool {
        let entry = ImageAddress::new(entry);
        !self.program.contains(&entry)
            && image
                .function_name(entry)
                .is_some_and(|name| name.starts_with("runtime."))
    }
}

/// The runtime's names for statuses and wait reasons, by number.
#[derive(Debug, Default)]
struct Names {
    statuses: Vec<Option<Arc<str>>>,
    wait_reasons: Vec<Option<Arc<str>>>,
}

impl Names {
    fn read(runtime: &GoRuntime, stop: &dyn RuntimeStop) -> Self {
        let strings = |name: &str| {
            let Some(symbol) = runtime.image.symbol(name) else {
                return Vec::new();
            };
            let Some(count) = symbol.size.map(|size| size / 16) else {
                return Vec::new();
            };
            let base = symbol.address.get().wrapping_add(stop.load_bias());
            (0..count.min(256))
                .map(|index| string(stop, VirtualAddress::new(base.wrapping_add(index * 16))))
                .collect()
        };
        Self {
            statuses: strings("runtime.gStatusStrings"),
            wait_reasons: strings("runtime.waitReasonStrings"),
        }
    }
}

fn read_unsigned(stop: &dyn RuntimeStop, address: VirtualAddress, size: usize) -> Option<u64> {
    let mut bytes = [0; 8];
    stop.read(address, bytes.get_mut(..size)?)
        .then(|| u64::from_le_bytes(bytes))
}

fn word(stop: &dyn RuntimeStop, address: VirtualAddress) -> Option<u64> {
    read_unsigned(stop, address, 8)
}

/// A Go string header's text, when it is short and readable.
fn string(stop: &dyn RuntimeStop, header: VirtualAddress) -> Option<Arc<str>> {
    let data = word(stop, header)?;
    let length = word(stop, VirtualAddress::new(header.get().wrapping_add(8)))?;
    let mut bytes = vec![
        0;
        usize::try_from(length)
            .ok()
            .filter(|length| *length <= 256)?
    ];
    stop.read(VirtualAddress::new(data), &mut bytes)
        .then_some(())?;
    String::from_utf8(bytes).ok().map(Arc::from)
}
