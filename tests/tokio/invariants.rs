//! What holds at every stop of a tokio program, whatever a test is about.
//! A scenario from [`checked`] runs [`check_tokio_stop`] after each stop,
//! so a bug surfaces in whichever test reaches it.
//!
//! - Every thread is accounted for: what it does for the runtime is known.
//! - The tasks are listed whole, each once. Only a list the program was
//!   changing at the stop may be stale, and the page says so, as it says
//!   when tokio's version is not the one verified.
//! - A thread runs a task exactly when the list puts the task on it, but
//!   for a worker's own launch, which the list puts on its worker.
//! - A thread running a task has tokio's dispatch among its frames.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use uscope::{
    Backtrace, CodeRole, DebuggerHandle, ExecutionContext, InferiorState, StackFrameId,
    StopContext, TaskSnapshot, ThreadActivity, ThreadId,
};

use crate::support::Scenario;

/// The most tasks a stop's check reads.
const MAX_TASKS: usize = 4096;

/// A scenario of `fixture` that checks every stop.
pub fn checked(fixture: &str) -> Scenario {
    Scenario::launch(fixture).checking_stops(check_tokio_stop)
}

/// Checks the invariants of the stop the debugger is at.
pub fn check_tokio_stop(
    handle: DebuggerHandle,
) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move { check(&handle).await })
}

/// A stop as the checks see it: each thread with what it does and its
/// backtrace, and the tasks with the gaps of their pages.
#[derive(Clone)]
struct Stop {
    threads: Vec<(ThreadId, ThreadActivity, Backtrace)>,
    tasks: Vec<TaskSnapshot>,
    gaps: Vec<String>,
}

async fn check(handle: &DebuggerHandle) -> Result<(), String> {
    read(handle).await?.map_or(Ok(()), |stop| check_stop(&stop))
}

/// The stop the debugger is at, or `None` when it is at none.
async fn read(handle: &DebuggerHandle) -> Result<Option<Stop>, String> {
    let failed = |what: &str, error: uscope::Error| format!("{what}: {error}");
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| failed("snapshot", error))?;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        return Ok(None);
    };
    let mut threads = Vec::new();
    for thread in snapshot.threads.iter() {
        let activity = thread
            .activity
            .clone()
            .ok_or_else(|| format!("thread {} has no activity", thread.id))?;
        let trace = handle
            .at(StopContext {
                stop: stop_id,
                execution: ExecutionContext::Thread(thread.id),
                frame: StackFrameId::INNERMOST,
            })
            .backtrace()
            .await
            .map_err(|error| failed(&format!("thread {}'s backtrace", thread.id), error))?;
        threads.push((thread.id, activity, trace));
    }
    let mut tasks = Vec::new();
    let mut gaps = Vec::new();
    let mut from = None;
    loop {
        let page = handle
            .tasks(from, 256)
            .await
            .map_err(|error| failed("tasks", error))?;
        tasks.extend(page.tasks.iter().cloned());
        gaps.extend(page.gaps.iter().map(ToString::to_string));
        match page.next {
            Some(next) if tasks.len() < MAX_TASKS => from = Some(next),
            _ => break,
        }
    }
    Ok(Some(Stop {
        threads,
        tasks,
        gaps,
    }))
}

fn check_stop(stop: &Stop) -> Result<(), String> {
    // A list may be changing, and tokio's version may be one the debugger
    // was not checked against; nothing else is missing.
    if let Some(gap) = stop.gaps.iter().find(|gap| {
        !gap.contains("was being changed at the stop")
            && !gap.contains("its runtime is read as tokio")
    }) {
        return Err(format!("the tasks are incomplete: {gap}"));
    }
    let mut ids = BTreeSet::new();
    if let Some(task) = stop.tasks.iter().find(|task| !ids.insert(task.id)) {
        return Err(format!("task {} is listed twice", task.id.number));
    }
    for (thread, activity, trace) in &stop.threads {
        match activity {
            ThreadActivity::Unknown(reason) => {
                return Err(format!("thread {thread} runs something unknown: {reason}"));
            }
            ThreadActivity::Task { task, .. } => {
                let listed = stop.tasks.iter().find(|listed| listed.id == *task);
                if listed.and_then(|listed| listed.thread) != Some(*thread) {
                    return Err(format!(
                        "thread {thread} runs task {}, which the list does not put on it: \
                         {listed:#?}",
                        task.number
                    ));
                }
                if !trace
                    .frames
                    .iter()
                    .any(|frame| frame.role == CodeRole::Dispatch)
                {
                    return Err(format!(
                        "thread {thread} runs task {} without tokio's dispatch: {trace:#?}",
                        task.number
                    ));
                }
            }
            ThreadActivity::Idle | ThreadActivity::Outside => {}
        }
    }
    for task in stop.tasks.iter().filter(|task| !task.internal) {
        let Some(thread) = task.thread else {
            continue;
        };
        let runs = stop.threads.iter().any(|(id, activity, _)| {
            *id == thread
                && matches!(activity, ThreadActivity::Task { task: on, .. } if *on == task.id)
        });
        if !runs {
            return Err(format!(
                "task {} is listed on thread {thread}, which does not run it",
                task.id.number
            ));
        }
    }
    Ok(())
}

/// The checks fail on the faults they look for: a task listed twice or
/// on a thread that does not run it, a thread running a task the list
/// leaves off it or without tokio's dispatch, an unknown thread, and a
/// gap other than a list changing.
#[tokio::test]
async fn the_checks_fail_on_the_faults_they_look_for() {
    let mut scenario = checked("tokio-workers-o0");
    scenario.add_breakpoint("task_reached").await;
    scenario.run_to_stop().await;
    let stop = read(scenario.handle())
        .await
        .expect("the stop")
        .expect("stopped");
    check_stop(&stop).expect("the stop holds");
    let (index, task) = stop
        .threads
        .iter()
        .enumerate()
        .find_map(|(index, (_, activity, _))| match activity {
            ThreadActivity::Task { task, .. } => Some((index, *task)),
            _ => None,
        })
        .expect("a thread runs a task");
    let position = stop
        .tasks
        .iter()
        .position(|listed| listed.id == task)
        .expect("the task is listed");

    let mut twice = stop.clone();
    twice.tasks.push(stop.tasks[position].clone());
    let mut moved = stop.clone();
    moved.tasks[position].thread = None;
    let mut undispatched = stop.clone();
    let frames = undispatched.threads[index]
        .2
        .frames
        .iter()
        .filter(|frame| frame.role != CodeRole::Dispatch)
        .cloned()
        .collect::<Vec<_>>();
    undispatched.threads[index].2.frames = frames.into();
    let mut idle = stop.clone();
    idle.threads[index].1 = ThreadActivity::Idle;
    let mut unknown = stop.clone();
    unknown.threads[index].1 = ThreadActivity::Unknown("unreadable".into());
    let mut gap = stop.clone();
    gap.gaps.push("shard 0 is broken".into());
    for (fault, sabotaged) in [
        ("twice", twice),
        ("moved", moved),
        ("undispatched", undispatched),
        ("idle", idle),
        ("unknown", unknown),
        ("gap", gap),
    ] {
        assert!(check_stop(&sabotaged).is_err(), "{fault}");
    }
    scenario.shutdown().await;
}
