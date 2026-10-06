//! A worker pool, and goroutines parked in every way a program parks one.

use std::collections::BTreeSet;

use uscope::{TaskSnapshot, TaskStack, TaskState, ThreadActivity, ThreadId};

use crate::truth::GoSession;

const BUILDS: [&str; 2] = ["workers-go-o0", "workers-go-o2"];

#[tokio::test]
async fn goroutines_are_listed_as_the_runtime_lists_them() {
    for fixture in BUILDS {
        let mut session = GoSession::launch(fixture).await;
        let truth = session.checkpoint("parked");
        // Small pages cross from one page to the next mid-list.
        let (tasks, gaps) = session.tasks(3).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");

        // The program's goroutines are exactly those of the runtime's dump,
        // which leaves out the runtime's own.
        let program = tasks
            .iter()
            .filter(|task| !task.internal)
            .collect::<Vec<_>>();
        let ids = program
            .iter()
            .map(|task| task.id.number)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), program.len(), "{fixture}: a task listed twice");
        assert_eq!(
            ids,
            truth.tasks.keys().copied().collect(),
            "{fixture}: {tasks:#?}"
        );
        assert_eq!(program.len(), truth.count, "{fixture}");
        assert!(
            tasks.len() > program.len(),
            "{fixture}: the runtime's own goroutines are listed too"
        );

        for task in &tasks {
            let context = format!("{fixture}: {task:#?}");
            let entry = task
                .entry
                .as_ref()
                .and_then(|entry| entry.function.as_deref())
                .expect(&context);
            assert_eq!(
                task.internal,
                entry.starts_with("runtime.") && entry != "runtime.main",
                "{context}"
            );
            let Some(dumped) = truth.tasks.get(&task.id.number) else {
                continue;
            };
            // The runtime describes a goroutine by what it waits for, or
            // by its status.
            assert_eq!(
                task.detail.as_deref(),
                Some(dumped.status.as_str()),
                "{context}"
            );
            let expected = match dumped.status.as_str() {
                "running" | "syscall" => TaskState::Running,
                "runnable" => TaskState::Runnable,
                _ => TaskState::Blocked,
            };
            assert_eq!(task.state, expected, "{context}");
            // Only a goroutine on a thread has one, and a parked one says
            // where it resumes.
            assert_eq!(
                task.thread.is_some(),
                expected == TaskState::Running,
                "{context}"
            );
            assert_eq!(task.resume.is_some(), task.thread.is_none(), "{context}");
            let created_by = task
                .creation
                .as_ref()
                .and_then(|creation| creation.function.as_deref());
            if task.id.number == truth.main.0 {
                assert_eq!(entry, "runtime.main", "{context}");
                assert_eq!(task.thread, Some(ThreadId::new(truth.main.1)), "{context}");
            } else {
                assert_eq!(created_by, Some("main.main"), "{context}");
                // `go worker(...)` begins in a wrapper the compiler writes
                // to pass the arguments; a closure begins in itself.
                let outermost = dumped.frames.last().map(|frame| frame.0.as_str());
                let started = if outermost == Some("main.worker") {
                    "main.main.gowrap1"
                } else {
                    outermost.expect(&context)
                };
                assert_eq!(entry, started, "{context}");
            }
        }
        check_threads(&mut session, &tasks, truth.main.0).await;
        session.scenario.shutdown().await;
    }
}

/// Every thread runs a goroutine or is idle, and a goroutine on a thread is
/// the one that thread runs. Another thread may be running the runtime's
/// code for its goroutine on the system stack, but the goroutine that hit
/// the breakpoint is on its own.
async fn check_threads(session: &mut GoSession, tasks: &[TaskSnapshot], main: u64) {
    let fixture = session.fixture.clone();
    {
        let snapshot = session.scenario.snapshot().await;
        let mut running = BTreeSet::new();
        for thread in snapshot.threads.iter() {
            let context = format!("{fixture}: {thread:#?}");
            match thread.activity.as_ref().expect(&context) {
                ThreadActivity::Task { task, stack } => {
                    if task.number == main {
                        assert_eq!(*stack, TaskStack::Own, "{context}");
                    }
                    let listed = tasks.iter().find(|listed| listed.id == *task);
                    assert_eq!(
                        listed.and_then(|listed| listed.thread),
                        Some(thread.id),
                        "{context}"
                    );
                    running.insert(task.number);
                }
                ThreadActivity::Idle => {}
                ThreadActivity::Unknown(reason) => panic!("{context}: {reason}"),
            }
        }
        let on_threads = tasks
            .iter()
            .filter(|task| task.thread.is_some())
            .map(|task| task.id.number)
            .collect::<BTreeSet<_>>();
        assert_eq!(running, on_threads, "{fixture}");
        assert!(running.contains(&main), "{fixture}");
    }
}
