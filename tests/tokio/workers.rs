//! tokio's tasks, found in the runtime's own memory: eight tasks parked at
//! different awaits, on a multi-thread runtime and a current-thread one,
//! with a blocking closure running and one queued, compared with what the
//! fixture reports at its checkpoint.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;

use uscope::{
    BreakpointSpec, InferiorState, LaunchOptions, StopReason, TaskPage, TaskSnapshot, TaskState,
    ThreadActivity, ThreadId,
};

use crate::stops::integer;
use crate::support::{Scenario, ScratchDir};

const BUILDS: [&str; 2] = ["tokio-workers-o0", "tokio-workers-o3"];

/// One build of the fixture, stopped at its first stop, with what it
/// printed going to a file.
struct Workers {
    scenario: Scenario,
    output: PathBuf,
    _scratch: ScratchDir,
}

impl Workers {
    /// The fixture on one runtime flavor, stopped at `spec`.
    async fn stopped_at(fixture: &str, current: bool, spec: BreakpointSpec) -> Self {
        let scratch = ScratchDir::new("workers");
        let output = scratch.path().join("stdout");
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint_spec(spec).await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: current.then(|| "current".into()).into_iter().collect(),
                stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        Self {
            scenario,
            output,
            _scratch: scratch,
        }
    }

    /// The fixture stopped at its checkpoint.
    async fn parked(fixture: &str, current: bool) -> Self {
        Self::stopped_at(
            fixture,
            current,
            BreakpointSpec::Function("truth_reached".into()),
        )
        .await
    }

    /// What the fixture reported at its checkpoint.
    fn truth(&self) -> Truth {
        let text = std::fs::read_to_string(&self.output).expect("the fixture's output");
        Truth::parse(&text)
    }

    /// Every task, read `page` at a time, and why the list may be
    /// incomplete.
    async fn tasks(&self, page: usize) -> (Vec<TaskSnapshot>, Vec<String>) {
        let mut tasks = Vec::new();
        let mut gaps = Vec::new();
        let mut from = None;
        loop {
            let TaskPage {
                tasks: found,
                next,
                gaps: missing,
                ..
            } = self
                .scenario
                .operation("tasks", self.scenario.handle().tasks(from, page))
                .await;
            assert!(found.len() <= page);
            tasks.extend(found.iter().cloned());
            gaps.extend(missing.iter().map(ToString::to_string));
            match next {
                Some(next) => from = Some(next),
                None => return (tasks, gaps),
            }
        }
    }

    /// What each thread does for the runtime.
    async fn activities(&mut self) -> BTreeMap<ThreadId, ThreadActivity> {
        let snapshot = self.scenario.snapshot().await;
        snapshot
            .threads
            .iter()
            .map(|thread| {
                let activity = thread
                    .activity
                    .clone()
                    .unwrap_or_else(|| panic!("thread {} has no activity", thread.id));
                (thread.id, activity)
            })
            .collect()
    }
}

/// What the fixture reports at its checkpoint.
#[derive(Debug, Default)]
struct Truth {
    /// Each async task's awaits, innermost first.
    tasks: BTreeMap<u64, Vec<String>>,
    /// The blocking closure running, and its thread.
    running: Option<(u64, ThreadId)>,
    queued: Option<u64>,
    /// Tasks the current-thread runtime has not polled since they were
    /// woken or spawned.
    woken: Option<u64>,
    spawned: Option<u64>,
    /// The thread that reached the checkpoint.
    main: Option<ThreadId>,
}

impl Truth {
    fn parse(text: &str) -> Self {
        let mut truth = Self::default();
        let number = |field: &str| field.parse::<u64>().expect("a number");
        let thread = |field: &str| ThreadId::new(number(field));
        for line in text.lines().filter_map(|line| line.strip_prefix("TRUTH\t")) {
            let fields = line.split('\t').collect::<Vec<_>>();
            match fields[..] {
                ["task", id, awaits, _] => {
                    let awaits = awaits.split(',').map(str::to_owned).collect();
                    truth.tasks.insert(number(id), awaits);
                }
                ["blocking", id, "running", tid] => truth.running = Some((number(id), thread(tid))),
                ["blocking", id, "queued"] => truth.queued = Some(number(id)),
                ["woken", id] => truth.woken = Some(number(id)),
                ["spawned", id] => truth.spawned = Some(number(id)),
                ["main", tid] => truth.main = Some(thread(tid)),
                _ => {}
            }
        }
        truth
    }

    /// Each task the program has, with the state, description, and thread
    /// the debugger must list it with.
    fn expected(&self) -> BTreeMap<u64, (TaskState, String, Option<ThreadId>)> {
        let mut expected = BTreeMap::new();
        for &id in self.tasks.keys() {
            expected.insert(id, (TaskState::Blocked, "suspended".into(), None));
        }
        for id in self.woken.iter().chain(&self.spawned) {
            expected.insert(*id, (TaskState::Runnable, "runnable".into(), None));
        }
        if let Some((id, thread)) = self.running {
            let running = "running a blocking closure".into();
            expected.insert(id, (TaskState::Running, running, Some(thread)));
        }
        if let Some(id) = self.queued {
            let queued = "queued in the blocking pool".into();
            expected.insert(id, (TaskState::Runnable, queued, None));
        }
        expected
    }

    /// Fails unless the list holds exactly the program's tasks, each once,
    /// as `expected` says.
    fn check_tasks(&self, tasks: &[TaskSnapshot]) -> Result<(), String> {
        let mut listed = BTreeMap::new();
        for task in tasks {
            let entry = (
                task.state.clone(),
                task.detail.as_deref().unwrap_or_default().to_owned(),
                task.thread,
            );
            if listed.insert(task.id.number, entry).is_some() {
                return Err(format!("task {} is listed twice", task.id.number));
            }
            if task.internal {
                return Err(format!("task {} is the runtime's own", task.id.number));
            }
        }
        let expected = self.expected();
        if listed == expected {
            Ok(())
        } else {
            Err(format!("listed {listed:#?}, expected {expected:#?}"))
        }
    }
}

/// Every task of either runtime flavor is listed once, in its state, with
/// nothing missing, across pages of any size.
async fn tasks_are_listed_exactly(current: bool) {
    for fixture in BUILDS {
        let mut workers = Workers::parked(fixture, current).await;
        let truth = workers.truth();
        assert_eq!(truth.tasks.len(), 8, "{fixture}: {truth:?}");
        // Small pages cross from one page to the next mid-shard.
        let (tasks, gaps) = workers.tasks(3).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");
        truth
            .check_tasks(&tasks)
            .unwrap_or_else(|problem| panic!("{fixture}: {problem}"));
        let (whole, _) = workers.tasks(4096).await;
        assert_eq!(whole, tasks, "{fixture}");

        // The check fails on a task left out, or in another state.
        let mut sabotaged = tasks.clone();
        sabotaged.pop();
        assert!(truth.check_tasks(&sabotaged).is_err(), "{fixture}");
        let mut sabotaged = tasks.clone();
        sabotaged[0].detail = Some("running".into());
        assert!(truth.check_tasks(&sabotaged).is_err(), "{fixture}");

        check_threads(&mut workers, &truth, &tasks).await;
        workers.scenario.shutdown().await;
    }
}

/// A thread runs a task only when the list says the task is on it: the
/// blocking pool's thread runs its closure's task, and every worker and
/// the thread at the checkpoint run none.
async fn check_threads(workers: &mut Workers, truth: &Truth, tasks: &[TaskSnapshot]) {
    let activities = workers.activities().await;
    let (blocking, thread) = truth.running.expect("a blocking closure runs");
    for (id, activity) in &activities {
        match activity {
            ThreadActivity::Task { task, .. } => {
                assert_eq!((task.number, *id), (blocking, thread), "{activities:#?}");
                let listed = tasks.iter().find(|listed| listed.id == *task);
                assert_eq!(listed.and_then(|listed| listed.thread), Some(*id));
            }
            ThreadActivity::Idle => {}
            ThreadActivity::Unknown(reason) => panic!("thread {id}: {reason}"),
        }
    }
    assert!(activities.contains_key(&truth.main.expect("the checkpoint's thread")));
}

#[tokio::test]
async fn a_multi_thread_runtimes_tasks_are_listed_exactly() {
    tasks_are_listed_exactly(false).await;
}

#[tokio::test]
async fn a_current_thread_runtimes_tasks_are_listed_exactly() {
    tasks_are_listed_exactly(true).await;
}

/// A task stopped at a breakpoint runs on the thread that hit it, whose
/// own id, `me`, is the task's number, and the thread runs that task.
async fn a_task_at_a_breakpoint_runs_on_its_thread(current: bool) {
    let at = BreakpointSpec::Function("task_reached".into());
    for fixture in BUILDS {
        let mut workers = Workers::stopped_at(fixture, current, at.clone()).await;
        let me = integer(&workers.scenario, "me")
            .await
            .and_then(|me| u64::try_from(me).ok())
            .unwrap_or_else(|| panic!("{fixture}: `me` is unavailable"));
        let InferiorState::Stopped {
            thread_id: stopped,
            ..
        } = workers.scenario.snapshot().await.inferior
        else {
            panic!("{fixture}: not stopped");
        };
        let (tasks, gaps) = workers.tasks(4096).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");
        let task = tasks
            .iter()
            .find(|task| task.id.number == me)
            .unwrap_or_else(|| panic!("{fixture}: task {me} is not listed: {tasks:#?}"));
        assert_eq!(task.state, TaskState::Running, "{fixture}: {task:#?}");
        assert_eq!(task.thread, Some(stopped), "{fixture}: {task:#?}");
        let activities = workers.activities().await;
        assert!(
            matches!(&activities[&stopped], ThreadActivity::Task { task, .. } if task.number == me),
            "{fixture}: {activities:#?}"
        );
        workers.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_task_at_a_breakpoint_runs_on_a_worker() {
    a_task_at_a_breakpoint_runs_on_its_thread(false).await;
}

#[tokio::test]
async fn a_task_at_a_breakpoint_runs_on_the_current_thread() {
    a_task_at_a_breakpoint_runs_on_its_thread(true).await;
}

/// Before `main` builds a runtime, no thread has entered one, and there
/// are no tasks and nothing missing.
#[tokio::test]
async fn before_any_runtime_there_are_no_tasks() {
    for fixture in BUILDS {
        let workers =
            Workers::stopped_at(fixture, false, BreakpointSpec::Function("main".into())).await;
        let (tasks, gaps) = workers.tasks(4096).await;
        assert!(tasks.is_empty() && gaps.is_empty(), "{fixture}: {tasks:#?} {gaps:?}");
        workers.scenario.shutdown().await;
    }
}

/// rustc describes each type once in every unit that uses it, so a large
/// program has more types than a small one by far, and a type defined in
/// many units is still one type.
#[tokio::test]
async fn every_type_of_a_large_program_is_read_and_named_once() {
    for fixture in BUILDS {
        let workers = Workers::parked(fixture, false).await;
        for (name, size) in [
            ("tokio::runtime::scheduler::multi_thread::worker::Shared", 296),
            ("tokio::runtime::scheduler::Handle", 16),
            ("tokio::runtime::task::core::Header", 32),
        ] {
            assert_eq!(
                integer(&workers.scenario, &format!("sizeof({name})")).await,
                Some(size),
                "{fixture}: {name}"
            );
        }
        workers.scenario.shutdown().await;
    }
}
