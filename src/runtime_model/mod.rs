//! What a language runtime tells the debugger at one stop: which tasks it
//! schedules, which task each thread runs, and where a parked task's frames
//! begin.
//!
//! Static facts about code, such as which functions switch stacks, belong
//! to the debug-info provider. Facts about one stop of one process belong
//! here. A runtime model is pure: it reaches a program only through
//! [`RuntimeImage`], the static facts of the module that carries the
//! runtime, and [`RuntimeStop`], one validated stop. A boundary test keeps
//! process control, debug-information parsing, I/O, clocks, and threads out
//! of it, and keeps every language's runtime in a module of its own.

mod go;

use std::sync::Arc;

use crate::unwind::RegisterFile;
use crate::{
    EntryProvenance, ImageAddress, IntegerValue, LanguageExceptionKind, ModuleImage,
    RecordMemberLayout, StackSegment, TaskState, ThreadId, ThreadLocal, TypeInfo, TypeKind,
    TypeNode, VirtualAddress,
};

/// A result with the reasons it may be incomplete, such as a task whose
/// memory could not be read. A result with no gaps is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial<T> {
    pub value: T,
    pub gaps: Vec<Arc<str>>,
}

/// Where a member lies within a named type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub offset: u64,
    pub size: u64,
}

/// A named object or function of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSymbol {
    pub address: ImageAddress,
    /// Its size in bytes, when the symbol table records one.
    pub size: Option<u64>,
}

/// The static facts of the module that carries a runtime, by name.
pub trait RuntimeImage: std::fmt::Debug {
    /// The producers of the module's debug information.
    fn producers(&self) -> &[Arc<str>];
    /// The value of a named integer constant.
    fn constant(&self, name: &str) -> Option<IntegerValue>;
    /// A named object or function.
    fn symbol(&self, name: &str) -> Option<ImageSymbol>;
    /// Where the named function's body begins, past the prologue that sets
    /// up its frame, or `None` when that is not known.
    fn function_body(&self, name: &str) -> Option<ImageAddress>;
    /// Where the member reached through `path` lies within a named record,
    /// through nested records.
    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member>;
    /// The name of the function whose code holds an image address.
    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>>;
    /// Where each thread's copy of the named thread-local variable is, or
    /// why that is unknown; `None` when the image defines none by the name.
    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>>;
}

/// One validated stop of the process a runtime runs in.
pub trait RuntimeStop {
    /// Fills `bytes` from memory as the program sees it, without the
    /// debugger's breakpoints, or returns `false` when any byte is
    /// unreadable.
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool;
    /// The thread pointer (`fs_base` on x86-64) of a stopped thread.
    fn thread_pointer(&self, thread: ThreadId) -> Option<u64>;
    /// The instruction a stopped thread will execute next.
    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress>;
    /// What the module carrying the runtime adds to its image addresses.
    fn load_bias(&self) -> u64;
}

/// An address in a task's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeAddress {
    pub address: VirtualAddress,
    /// Whether it is a return address, which names the call before it.
    pub after_call: bool,
}

/// One task of a runtime at a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTask {
    /// The runtime's number for the task.
    pub number: u64,
    pub state: TaskState,
    /// The runtime's own words for what the task waits for.
    pub detail: Option<Arc<str>>,
    /// The thread running the task, or the runtime's code for it, as while
    /// it makes a system call or is being parked.
    pub thread: Option<ThreadId>,
    /// Where the task will resume, for one that is not on a thread.
    pub resume: Option<CodeAddress>,
    /// The call that created the task.
    pub creation: Option<CodeAddress>,
    /// The function the task began in.
    pub entry: Option<VirtualAddress>,
    /// The task that created this one.
    pub parent: Option<u64>,
    /// Whether the runtime runs the task for its own work, such as a
    /// garbage collector's worker.
    pub internal: bool,
    /// The key-value labels the program gave the task, in the runtime's
    /// order, such as Go's profiler labels.
    pub labels: TaskLabels,
}

/// A task's labels: keys and their values.
pub type TaskLabels = Vec<(Arc<str>, Arc<str>)>;

/// The runtime's tasks in one range of its own order, and where the next
/// range begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<RuntimeTask>,
    /// Where the next page begins, or `None` after the last.
    pub next: Option<u64>,
}

/// What a stopped thread is doing for the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadActivity {
    /// Running a task, or the runtime's code on its behalf.
    Task { number: u64, stack: StackSegment },
    /// Running the runtime's scheduler with no task, or code the runtime
    /// does not know, such as a thread C created.
    Idle,
    /// The runtime's state for the thread could not be read.
    Unknown(Arc<str>),
}

/// Where a task's frames begin.
#[derive(Debug, Clone)]
pub enum TaskContext {
    /// The task runs on a thread, whose registers begin its frames.
    OnThread(ThreadId),
    /// The task is parked with these registers saved; any other register is
    /// unknown.
    Saved {
        registers: RegisterFile,
        /// Whether the saved instruction is a return address, which names
        /// the call before it, as when a task parked by calling into its
        /// runtime.
        after_call: bool,
    },
}

/// Where unwinding goes from a frame whose code switches stacks.
#[derive(Debug, Clone)]
pub enum Crossing {
    /// The frame is on the stack it was called on, and unwinds as any
    /// other frame does.
    Stay,
    /// The frame's own stack pointer is elsewhere than its registers say,
    /// as for a frame called on a task's stack that runs on the system
    /// stack. It unwinds as any other frame does, from these registers.
    Resume(RegisterFile),
    /// The frame never returns to its caller. The registers the task it
    /// switched from saved begin the next frame, whose instruction is a
    /// return address.
    Continue(RegisterFile),
    /// The frame is the first of its stack: no task's frames lie beyond it.
    Outermost,
}

/// What each runtime a model knows calls its tasks, singular and plural,
/// so that clients can speak of them as the runtime's users do.
pub const TASK_NOUNS: [(&str, &str); 1] = [go::TASK_NOUN];

/// How a runtime uses the process's signals, by Linux signal number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeSignals {
    /// Signals the runtime tolerates arriving late. One that arrives while
    /// the debugger runs a thread alone for an instruction waits for the
    /// thread's next continue, rather than running a handler that may wait
    /// for threads the debugger holds.
    pub deferrable: &'static [i32],
    /// Signals the runtime handles as part of the program's own work, such
    /// as the faults Go turns into panics. By default they neither stop
    /// nor print, and are delivered; the runtime reports any it cannot
    /// handle as an exception of its own.
    pub handled: &'static [i32],
}

/// A runtime function that reports an exception as it is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeHook {
    pub kind: LanguageExceptionKind,
    pub address: ImageAddress,
}

/// What a runtime reports as one of its hooks is entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeException {
    /// The runtime's message, as the runtime would print it.
    pub message: Arc<str>,
    /// An expression for the value the exception carries, when one can
    /// name it.
    pub value: Option<Arc<str>>,
}

/// Where a value whose dynamic type a runtime records is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredValue<'a> {
    /// In the program's memory.
    Memory(VirtualAddress),
    /// In these bytes, captured from registers or memory.
    Bytes(&'a [u8]),
}

/// What a value of a type whose dynamic type the runtime records holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicValue {
    /// Nothing, as a nil interface holds.
    Nil,
    /// A value of the type the runtime describes at `descriptor`.
    Held {
        descriptor: VirtualAddress,
        /// The descriptor's offset in the type table of the module holding
        /// it, which that module's debug information names its types by.
        offset: u64,
        place: HeldPlace,
    },
}

/// Where a held value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldPlace {
    /// In the program's memory.
    Memory(VirtualAddress),
    /// In the holding value's own bytes, this far in.
    Within(u64),
}

/// What a language runtime tells the debugger at a stop.
pub trait RuntimeModel: Send + Sync + std::fmt::Debug {
    /// The runtime's tasks from `start`, an index into its own order, at
    /// most `limit` of them.
    fn tasks(&self, stop: &dyn RuntimeStop, start: u64, limit: usize) -> Partial<TaskPage>;
    /// What a stopped thread is doing for the runtime.
    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity;
    /// Where the frames of the task numbered `number` begin, or `None` when
    /// the runtime has no such task.
    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        number: u64,
    ) -> Result<Option<TaskContext>, Arc<str>>;
    /// The stacks a stopped thread may run on for the runtime, each with
    /// whose it is: the bounds of the stack of the task it runs, and of the
    /// runtime's own stacks for it.
    fn thread_stacks(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>>;
    /// Where unwinding goes from a frame of a stopped thread whose code
    /// switches stacks, given the frame's registers. `after_call` says the
    /// frame's instruction is a return address, as every frame's is but
    /// the innermost and one a signal interrupted: the frame is in a call.
    fn cross(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: &RegisterFile,
        after_call: bool,
    ) -> Result<Crossing, Arc<str>>;
    /// How the runtime uses signals.
    fn signals(&self) -> RuntimeSignals;
    /// The functions that report the runtime's exceptions as they are
    /// entered.
    fn hooks(&self) -> &[RuntimeHook];
    /// What a stopped thread reports as it enters the hook at `hook`,
    /// given the thread's registers there.
    fn exception(
        &self,
        stop: &dyn RuntimeStop,
        hook: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>>;
    /// What the value stored at `value` dynamically holds, when the record
    /// its type is represented by, named `representation`, is one the
    /// runtime records a dynamic type in; `None` for any other record.
    fn dynamic_value(
        &self,
        stop: &dyn RuntimeStop,
        representation: &str,
        value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>>;
    /// The runtime function that moves a task's stack to another, which
    /// the move has finished once it returns.
    fn stack_mover(&self) -> Option<ImageAddress>;
    /// The number of the task whose stack a stopped thread entering the
    /// stack mover, given its registers there, is moving.
    fn moving_task(
        &self,
        stop: &dyn RuntimeStop,
        registers: &RegisterFile,
    ) -> Result<u64, Arc<str>>;
    /// The bounds of the stack of the task numbered `number`, or `None`
    /// when the runtime has no such task.
    fn task_stack(
        &self,
        stop: &dyn RuntimeStop,
        number: u64,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>>;
    /// The program's code that the runtime function at `entry` goes on to
    /// call for the program, as Go's calls between Go and C do, given the
    /// registers of a stopped thread entering it; `None` when it calls
    /// none.
    fn call_out(
        &self,
        entry: ImageAddress,
        registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>>;
    /// What the runtime calls one of its tasks.
    fn task_noun(&self) -> &'static str;
}

/// The runtime a module carries, bound against its debug information, or
/// why it cannot be; `None` for a module with no runtime model knows.
pub fn detect(
    image: Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Arc<str>>> {
    go::detect(image)
}

impl RuntimeImage for ModuleImage {
    fn producers(&self) -> &[Arc<str>] {
        Self::producers(self)
    }

    fn constant(&self, name: &str) -> Option<IntegerValue> {
        Self::constant(self, name)
    }

    fn symbol(&self, name: &str) -> Option<ImageSymbol> {
        let symbol = self.symbol_named(name).ok()?;
        let size = symbol
            .extent
            .map(|extent| extent.range)
            .or(symbol.storage)
            .map(|range| range.end.get() - range.start.get());
        Some(ImageSymbol {
            address: symbol.address,
            size,
        })
    }

    /// Where a function breakpoint enters the function, once that is past
    /// its first instruction.
    fn function_body(&self, name: &str) -> Option<ImageAddress> {
        let entry = self.symbol_named(name).ok()?.address;
        let instance = self.locate(entry).physical_instance?;
        self.recommended_entries_for_instance(instance)
            .filter(|body| {
                matches!(
                    body.provenance,
                    EntryProvenance::AnalyzedPrologue | EntryProvenance::Statement
                ) && body.address > entry
            })
            .map(|body| body.address)
            .min()
    }

    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member> {
        let mut ty = self.types().iter().find_map(|node| match node {
            TypeNode::Resolved(info) if info.name.as_ref() == type_name => Some(info),
            _ => None,
        })?;
        let mut offset = 0_u64;
        for name in path {
            let TypeKind::Record { members, .. } = &representation(self, ty)?.kind else {
                return None;
            };
            let member = members
                .iter()
                .find(|member| member.name.as_deref() == Some(*name))?;
            let RecordMemberLayout::ByteOffset(at) = member.layout else {
                return None;
            };
            offset = offset.checked_add(at)?;
            ty = self.type_info(member.type_ref)?;
        }
        Some(Member {
            offset,
            size: representation(self, ty)?.byte_size?,
        })
    }

    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        Self::thread_local(self, name)
    }

    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>> {
        let location = self.locate(address);
        location
            .physical_instance
            .and_then(|instance| self.code_instance(instance))
            .and_then(|instance| self.function(instance.function))
            .map(|function| Arc::clone(&function.name))
            .or_else(|| self.symbolize(address).map(|symbol| symbol.name))
    }
}

/// The type that lays out a value of `ty`, through names and qualifiers.
fn representation<'a>(image: &'a ModuleImage, mut ty: &'a TypeInfo) -> Option<&'a TypeInfo> {
    for _ in 0..16 {
        match &ty.kind {
            TypeKind::Named { target, .. } => ty = image.type_info((*target)?)?,
            TypeKind::Modified { target, .. } => ty = image.type_info(*target)?,
            _ => return Some(ty),
        }
    }
    None
}

#[cfg(test)]
mod tests;
