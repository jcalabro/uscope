//! The language runtimes in a stopped process, and the tasks they schedule.
//!
//! Each loaded image is asked once whether it carries a runtime a model
//! knows; the bound model then answers through the stopped process's memory
//! and threads, read as every other inspection reads them.

use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::StopId;
use crate::runtime_model::{self, CodeAddress, RuntimeModel, RuntimeStop, RuntimeTask};
use crate::{
    Error, ImageAddress, LoadedModule, ModuleImage, Result, RuntimeId, TaskCursor, TaskId,
    TaskLocation, TaskPage, TaskSnapshot, ThreadActivity, ThreadId, VirtualAddress,
};

use super::memory::read_logical_memory;
use super::native::InspectionOps;
use super::{
    BreakpointSite, Controller, Inferior, debug_pid, debug_thread_id, validate_public_stop,
};

/// The most tasks one page holds.
pub(super) const MAX_TASK_PAGE: usize = 4096;

/// A runtime bound to the loaded module that carries it.
#[derive(Clone)]
pub(super) struct BoundRuntime {
    pub(super) id: RuntimeId,
    pub(super) module: LoadedModule,
    pub(super) model: Arc<dyn RuntimeModel>,
}

/// The models bound to each image so far: `None` for an image with no
/// runtime, or why binding one failed.
pub(super) type RuntimeCache = std::cell::RefCell<
    std::collections::BTreeMap<
        crate::ModuleImageId,
        Option<std::result::Result<Arc<dyn RuntimeModel>, Arc<str>>>,
    >,
>;

/// One stop of the process, as a runtime model reads it.
struct ProcessStop<'a, P> {
    ptrace: &'a P,
    /// A stopped thread, through which memory is read.
    reader: Pid,
    breakpoints: &'a std::collections::BTreeMap<VirtualAddress, BreakpointSite>,
    bias: u64,
}

impl<P: InspectionOps> RuntimeStop for ProcessStop<'_, P> {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        match read_logical_memory(
            self.ptrace,
            self.reader,
            self.breakpoints,
            address,
            bytes.len(),
        ) {
            Ok(read) if read.bytes.len() == bytes.len() => {
                bytes.copy_from_slice(&read.bytes);
                true
            }
            _ => false,
        }
    }

    fn thread_pointer(&self, thread: ThreadId) -> Option<u64> {
        let pid = debug_pid(thread).ok()?;
        self.ptrace
            .registers(pid)
            .ok()
            .map(|registers| registers.fs_base)
    }

    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress> {
        let pid = debug_pid(thread).ok()?;
        self.ptrace
            .registers(pid)
            .ok()
            .map(|registers| VirtualAddress::new(registers.rip))
    }

    fn load_bias(&self) -> u64 {
        self.bias
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Every loaded module carrying a runtime a model knows, with the model
    /// bound to it. A module whose runtime cannot be bound has none.
    pub(super) fn runtimes(&self, inferior: &Inferior) -> Vec<BoundRuntime> {
        let images = std::iter::once((inferior.loaded_module, &self.module_image)).chain(
            self.modules
                .values()
                .filter(|module| module.loaded.id != inferior.loaded_module.id)
                .map(|module| (module.loaded, &module.image)),
        );
        images
            .filter_map(|(module, image)| {
                let model = self.bind_runtime(image)?.ok()?;
                Some(BoundRuntime {
                    id: RuntimeId::new(module.id.index() as u64),
                    module,
                    model,
                })
            })
            .collect()
    }

    fn bind_runtime(
        &self,
        image: &Arc<ModuleImage>,
    ) -> Option<std::result::Result<Arc<dyn RuntimeModel>, Arc<str>>> {
        self.runtime_models
            .borrow_mut()
            .entry(image.id())
            .or_insert_with(|| runtime_model::detect(Arc::clone(image) as _))
            .clone()
    }

    /// Runs `read` against one runtime at the current stop.
    pub(super) fn with_runtime_stop<T>(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        read: impl FnOnce(&dyn RuntimeStop) -> T,
    ) -> Result<T> {
        let stop = inferior.public_stop.as_ref().ok_or(Error::NotStopped)?;
        Ok(read(&ProcessStop {
            ptrace: &self.ptrace,
            reader: stop.triggering_thread,
            breakpoints: &inferior.breakpoints,
            bias: runtime.module.load_bias,
        }))
    }

    /// One page of the tasks of every runtime at a stop.
    pub(super) fn tasks(
        &self,
        stop_id: StopId,
        from: Option<TaskCursor>,
        limit: usize,
    ) -> Result<TaskPage> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let limit = limit.clamp(1, MAX_TASK_PAGE);
        let runtimes = self.runtimes(inferior);
        let mut cursor = from.unwrap_or(TaskCursor {
            runtime: 0,
            position: 0,
        });
        let mut tasks = Vec::new();
        let mut gaps = Vec::new();
        while let Some(runtime) = runtimes.get(cursor.runtime) {
            let page = self.with_runtime_stop(inferior, runtime, |stop| {
                runtime
                    .model
                    .tasks(stop, cursor.position, limit - tasks.len())
            })?;
            gaps.extend(page.gaps);
            tasks.extend(
                page.value
                    .tasks
                    .iter()
                    .map(|task| self.task_snapshot(inferior, runtime, task)),
            );
            cursor = page.value.next.map_or(
                TaskCursor {
                    runtime: cursor.runtime + 1,
                    position: 0,
                },
                |position| TaskCursor {
                    runtime: cursor.runtime,
                    position,
                },
            );
            if tasks.len() == limit {
                break;
            }
        }
        Ok(TaskPage {
            tasks: tasks.into(),
            next: (cursor.runtime < runtimes.len()).then_some(cursor),
            gaps: gaps.into(),
        })
    }

    fn task_snapshot(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        task: &RuntimeTask,
    ) -> TaskSnapshot {
        let id = |number| TaskId {
            runtime: runtime.id,
            number,
        };
        let code = |code: CodeAddress| self.task_location(inferior, code);
        TaskSnapshot {
            id: id(task.number),
            state: task.state.clone(),
            detail: task.detail.clone(),
            thread: task.thread,
            resume: task.resume.map(code),
            creation: task.creation.map(code),
            entry: task.entry.map(|address| {
                code(CodeAddress {
                    address,
                    after_call: false,
                })
            }),
            parent: task.parent.map(id),
            internal: task.internal,
        }
    }

    /// The function and source line of an address in a task's code.
    fn task_location(&self, inferior: &Inferior, code: CodeAddress) -> TaskLocation {
        let lookup = if code.after_call {
            VirtualAddress::new(code.address.get().saturating_sub(1))
        } else {
            code.address
        };
        let found = std::iter::once((inferior.loaded_module, &self.module_image))
            .chain(
                self.modules
                    .values()
                    .map(|module| (module.loaded, &module.image)),
            )
            .find_map(|(module, image)| {
                let address: ImageAddress = module.image_address(lookup).ok()?;
                image
                    .contains_address(address)
                    .then(|| (module, image.locate(address)))
            });
        let Some((module, location)) = found else {
            return TaskLocation {
                address: code.address,
                module: None,
                function: None,
                source: None,
            };
        };
        TaskLocation {
            address: code.address,
            module: Some(module.id),
            function: location
                .function
                .map(|function| function.name)
                .or_else(|| location.symbol.map(|symbol| symbol.name)),
            source: location.source,
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    /// What a stopped thread runs for the process's runtimes: the first
    /// task one names, or why one could not tell; `None` without a runtime.
    pub(super) fn thread_activity(&self, inferior: &Inferior, pid: Pid) -> Option<ThreadActivity> {
        let stop = inferior.public_stop.as_ref()?;
        if let Some(known) = stop.activities.borrow().get(&pid) {
            return known.clone();
        }
        let activity = self.read_thread_activity(inferior, pid);
        stop.activities.borrow_mut().insert(pid, activity.clone());
        activity
    }

    fn read_thread_activity(&self, inferior: &Inferior, pid: Pid) -> Option<ThreadActivity> {
        let runtimes = self.runtimes(inferior);
        let mut found = (!runtimes.is_empty()).then_some(ThreadActivity::Idle);
        for runtime in &runtimes {
            let activity = self
                .with_runtime_stop(inferior, runtime, |stop| {
                    runtime.model.thread_activity(stop, debug_thread_id(pid))
                })
                .ok()?;
            match activity {
                runtime_model::ThreadActivity::Task { number, stack } => {
                    return Some(ThreadActivity::Task {
                        task: TaskId {
                            runtime: runtime.id,
                            number,
                        },
                        stack,
                    });
                }
                runtime_model::ThreadActivity::Idle => {}
                runtime_model::ThreadActivity::Unknown(reason) => {
                    found = Some(ThreadActivity::Unknown(reason));
                }
            }
        }
        found
    }
}
