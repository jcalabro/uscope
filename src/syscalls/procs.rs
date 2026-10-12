//! The traced processes and threads: who made whom, what each runs, and
//! how each ended, from the trace's spawn, exec, and exit events.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::model::{ProcessEnd, TraceEvent};

/// One traced process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Process {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Images it ran, in order, with when each exec happened.
    pub images: Vec<(u64, Arc<str>)>,
    pub threads: Vec<u32>,
    /// When the trace first saw it.
    pub started: u64,
    pub ended: Option<(u64, ProcessEnd)>,
}

/// Every process a trace saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessTree {
    processes: BTreeMap<u32, Process>,
    /// Each thread's process.
    threads: BTreeMap<u32, u32>,
}

impl ProcessTree {
    /// Records a process the trace began with, as a launched program or an
    /// attached process.
    pub fn root(&mut self, pid: u32, time: u64, image: Option<Arc<str>>) {
        let process = self.processes.entry(pid).or_insert_with(|| Process {
            pid,
            started: time,
            threads: vec![pid],
            ..Process::default()
        });
        if let Some(image) = image {
            process.images.push((time, image));
        }
        self.threads.insert(pid, pid);
    }

    pub fn apply(&mut self, event: &TraceEvent) {
        match *event {
            TraceEvent::Spawn {
                parent,
                child,
                thread,
                time,
                ..
            } => {
                if thread {
                    self.threads.insert(child, parent);
                    let process = self.processes.entry(parent).or_insert_with(|| Process {
                        pid: parent,
                        started: time,
                        ..Process::default()
                    });
                    if !process.threads.contains(&child) {
                        process.threads.push(child);
                    }
                } else {
                    let images = self
                        .processes
                        .get(&parent)
                        .and_then(|process| process.images.last().cloned())
                        .into_iter()
                        .collect();
                    self.processes.insert(
                        child,
                        Process {
                            pid: child,
                            parent: Some(parent),
                            images,
                            threads: vec![child],
                            started: time,
                            ended: None,
                        },
                    );
                    self.threads.insert(child, child);
                }
            }
            TraceEvent::Exec {
                process,
                time,
                ref path,
                ..
            } => {
                let entry = self.processes.entry(process).or_insert_with(|| Process {
                    pid: process,
                    started: time,
                    threads: vec![process],
                    ..Process::default()
                });
                entry.images.push((time, path.clone()));
                // Every other thread is gone after an exec.
                entry.threads.retain(|&thread| thread == process);
            }
            TraceEvent::Exit { process, time, end } => {
                if let Some(entry) = self.processes.get_mut(&process) {
                    entry.ended = Some((time, end));
                }
            }
            TraceEvent::Call(_) | TraceEvent::Signal { .. } | TraceEvent::Lost { .. } => {}
        }
    }

    /// The process a thread belongs to, when the trace knows.
    #[must_use]
    pub fn process_of(&self, thread: u32) -> Option<u32> {
        self.threads.get(&thread).copied()
    }

    #[must_use]
    pub fn get(&self, pid: u32) -> Option<&Process> {
        self.processes.get(&pid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Process> {
        self.processes.values()
    }

    /// Processes that have not ended.
    pub fn live(&self) -> impl Iterator<Item = u32> + '_ {
        self.processes
            .values()
            .filter(|process| process.ended.is_none())
            .map(|process| process.pid)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forks_execs_and_exits_build_the_tree() {
        let mut tree = ProcessTree::default();
        tree.root(10, 0, Some("/bin/sh".into()));
        tree.apply(&TraceEvent::Spawn {
            parent_thread: 10,
            parent: 10,
            child: 11,
            thread: false,
            time: 1,
        });
        tree.apply(&TraceEvent::Spawn {
            parent_thread: 10,
            parent: 10,
            child: 12,
            thread: true,
            time: 2,
        });
        tree.apply(&TraceEvent::Exec {
            thread: 11,
            process: 11,
            time: 3,
            path: "/bin/ls".into(),
        });
        tree.apply(&TraceEvent::Exit {
            process: 11,
            time: 4,
            end: ProcessEnd::Exited(0),
        });
        let child = tree.get(11).expect("child");
        assert_eq!(child.parent, Some(10));
        assert_eq!(
            child
                .images
                .iter()
                .map(|(_, path)| &**path)
                .collect::<Vec<_>>(),
            ["/bin/sh", "/bin/ls"]
        );
        assert_eq!(child.ended, Some((4, ProcessEnd::Exited(0))));
        assert_eq!(tree.process_of(12), Some(10));
        assert_eq!(tree.live().collect::<Vec<_>>(), [10]);
    }
}
