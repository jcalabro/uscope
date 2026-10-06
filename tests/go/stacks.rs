//! Stacks that continue from a thread's system stack onto a goroutine's
//! own, across each of the runtime's stack switches.

use uscope::{
    Backtrace, BreakpointSpec, CodeRole, Evaluation, ExecutionContext, Expression, InferiorState,
    ScalarValue, StackFrameId, StackSegment, StopContext, StopId, StopReason, ThreadActivity,
    ThreadId, ThreadState, UnwindTermination, VariableState, VariableValue, VirtualAddress,
};

use crate::support::Scenario;

const BUILDS: [&str; 2] = ["stacks-go-o0", "stacks-go-o2"];

/// The most stops a test passes over before the one it waits for.
const MAX_STOPS: usize = 200;

/// A backtrace's function names, grouped by the stack each run of frames
/// is on, innermost first.
fn segments(trace: &Backtrace) -> Vec<(StackSegment, Vec<String>)> {
    let mut segments: Vec<(StackSegment, Vec<String>)> = Vec::new();
    for frame in trace.frames.iter() {
        let name = frame
            .function
            .as_ref()
            .map_or_else(|| "?".to_owned(), |function| function.name.to_string());
        match segments.last_mut() {
            Some((segment, names)) if *segment == frame.segment => names.push(name),
            _ => segments.push((frame.segment, vec![name])),
        }
    }
    segments
}

/// Runs a fixture to a breakpoint on `function`, then on until `wanted`
/// accepts the backtrace of a thread the breakpoint stopped. Threads that
/// hit it together stop together, each with its own reason.
async fn stop_in(
    fixture: &str,
    function: &str,
    wanted: impl Fn(&[(StackSegment, Vec<String>)]) -> bool,
) -> (Scenario, Backtrace) {
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint(function).await;
    let reason = scenario.run_to_stop().await;
    let trace = stop_where(&mut scenario, reason, fixture, wanted).await;
    (scenario, trace)
}

/// From a breakpoint stop, resumes until `wanted` accepts the backtrace of
/// a thread a breakpoint stopped.
async fn stop_where(
    scenario: &mut Scenario,
    mut reason: StopReason,
    fixture: &str,
    wanted: impl Fn(&[(StackSegment, Vec<String>)]) -> bool,
) -> Backtrace {
    for _ in 0..MAX_STOPS {
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        let snapshot = scenario.snapshot().await;
        let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
            panic!("{fixture}: not stopped");
        };
        let hit = snapshot.threads.iter().filter(|thread| {
            matches!(
                thread.state,
                ThreadState::Stopped {
                    reason: Some(StopReason::Breakpoint { .. })
                }
            )
        });
        for thread in hit {
            let trace = scenario
                .operation(
                    "backtrace",
                    scenario
                        .handle()
                        .at(innermost(stop_id, thread.id))
                        .backtrace(),
                )
                .await;
            if wanted(&segments(&trace)) {
                return trace;
            }
        }
        reason = scenario.resume_to_stop().await;
    }
    panic!("{fixture}: no stop was the one sought");
}

const fn innermost(stop: StopId, thread: ThreadId) -> StopContext {
    StopContext {
        stop,
        execution: ExecutionContext::Thread(thread),
        frame: StackFrameId::INNERMOST,
    }
}

#[tokio::test]
async fn system_stack_calls_unwind_onto_their_goroutine() {
    for fixture in BUILDS {
        let (mut scenario, trace) = stop_in(fixture, "runtime.readmemstats_m", |_| true).await;
        assert_eq!(
            segments(&trace),
            [
                (
                    StackSegment::System,
                    names(&[
                        "runtime.readmemstats_m",
                        "runtime.ReadMemStats.func1",
                        "runtime.systemstack",
                    ])
                ),
                (
                    StackSegment::Task,
                    names(&[
                        "runtime.ReadMemStats",
                        "main.stats",
                        "main.main",
                        "runtime.main",
                        "runtime.goexit",
                    ])
                ),
            ],
            "{fixture}: {trace:#?}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{fixture}");
        check_every_thread(&mut scenario, fixture).await;
        scenario.shutdown().await;
    }
}

/// With the world stopped, every thread's stack unwinds to its first
/// frame, and a thread running the runtime's code for a goroutine goes on
/// to the goroutine's stack.
async fn check_every_thread(scenario: &mut Scenario, fixture: &str) {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("{fixture}: not stopped");
    };
    for thread in snapshot.threads.iter() {
        let trace = scenario
            .operation(
                "thread backtrace",
                scenario
                    .handle()
                    .at(innermost(stop_id, thread.id))
                    .backtrace(),
            )
            .await;
        let context = format!("{fixture}: {:?}\n{trace:#?}", thread.activity);
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        let stacks = segments(&trace)
            .into_iter()
            .map(|(segment, _)| segment)
            .collect::<Vec<_>>();
        let expected = match thread.activity.as_ref().expect(&context) {
            ThreadActivity::Task { stack, .. } if *stack == StackSegment::Task => {
                vec![StackSegment::Task]
            }
            ThreadActivity::Task { stack, .. } => vec![*stack, StackSegment::Task],
            ThreadActivity::Idle => vec![StackSegment::Thread],
            // A thread the runtime is still starting has no goroutine yet,
            // and its stack begins where `clone` made it.
            ThreadActivity::Unknown(_) => {
                let innermost = trace
                    .frames
                    .first()
                    .and_then(|frame| frame.function.as_ref());
                assert_eq!(
                    innermost.map(|function| function.name.as_ref()),
                    Some("runtime.clone"),
                    "{context}"
                );
                vec![StackSegment::Thread]
            }
        };
        assert_eq!(stacks, expected, "{context}");
        if expected.last() == Some(&StackSegment::Task) {
            let outermost = trace
                .frames
                .last()
                .and_then(|frame| frame.function.as_ref());
            assert_eq!(
                outermost.map(|function| function.name.as_ref()),
                Some("runtime.goexit"),
                "{context}"
            );
        }
    }
}

#[tokio::test]
async fn a_growing_stack_unwinds_onto_its_goroutine() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.newstack", |segments| {
            segments
                .get(1)
                .is_some_and(|(_, names)| names.first().is_some_and(|name| name == "main.grow"))
        })
        .await;
        let found = segments(&trace);
        let context = format!("{fixture}: {trace:#?}");
        assert_eq!(
            found[0],
            (
                StackSegment::System,
                names(&["runtime.newstack", "runtime.morestack"])
            ),
            "{context}"
        );
        // The goroutine is in the prologue of the call that needed more
        // stack, below every call before it.
        let (segment, task) = &found[1];
        assert_eq!(*segment, StackSegment::Task, "{context}");
        let calls = task.iter().take_while(|name| *name == "main.grow").count();
        assert!(calls > 1, "{context}");
        assert_eq!(
            task[calls..],
            names(&["main.main.func2", "runtime.goexit"]),
            "{context}"
        );
        assert_eq!(found.len(), 2, "{context}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_parking_goroutine_unwinds_onto_its_goroutine() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.park_m", |segments| {
            segments
                .get(1)
                .is_some_and(|(_, names)| names.iter().any(|name| name == "main.main.func1"))
        })
        .await;
        let context = format!("{fixture}: {trace:#?}");
        assert_eq!(
            segments(&trace),
            [
                (
                    StackSegment::System,
                    names(&["runtime.park_m", "runtime.mcall"])
                ),
                (
                    StackSegment::Task,
                    names(&[
                        "runtime.gopark",
                        "runtime.chanrecv",
                        "runtime.chanrecv1",
                        "main.main.func1",
                        "runtime.goexit",
                    ])
                ),
            ],
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_thread_switching_to_a_goroutine_unwinds_on_its_system_stack() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "gogo", |_| true).await;
        let found = segments(&trace);
        let context = format!("{fixture}: {trace:#?}");
        let [(segment, names)] = found.as_slice() else {
            panic!("{context}");
        };
        // The scheduler has made the goroutine the thread's own before it
        // switches to it.
        assert_eq!(*segment, StackSegment::System, "{context}");
        assert_eq!(names.first().map(String::as_str), Some("gogo"), "{context}");
        assert!(
            names.iter().any(|name| name == "runtime.schedule"),
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

/// The runtime reads the clock through the vDSO from the system stack, and
/// keeps the goroutine's stack pointer in a register the vDSO preserves.
#[tokio::test]
async fn a_vdso_call_unwinds_onto_its_caller() {
    for fixture in BUILDS {
        let mut scenario = Scenario::launch(fixture);
        let start = scenario.add_breakpoint("main.stats").await;
        scenario.run_to_stop().await;
        // The program holds the vDSO's clock_gettime itself.
        let expression = Expression::parse("runtime.vdsoClockgettimeSym").expect("an expression");
        let clock = scenario
            .operation("vDSO clock", scenario.handle().evaluate(&expression))
            .await;
        let Evaluation::Value { value, .. } = clock else {
            panic!("{fixture}: {clock:?}");
        };
        let VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(address)),
            ..
        } = value.state
        else {
            panic!("{fixture}: {value:?}");
        };
        scenario.remove_breakpoint(start.id).await;
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(
                u64::try_from(address).expect("an address"),
            )))
            .await;
        let reason = scenario.resume_to_stop().await;
        let trace = stop_where(&mut scenario, reason, fixture, |segments| {
            segments
                .iter()
                .any(|(_, names)| names.iter().any(|name| name == "runtime.nanotime1"))
        })
        .await;
        let context = format!("{fixture}: {trace:#?}");
        let functions = trace
            .frames
            .iter()
            .map(|frame| {
                frame
                    .function
                    .as_ref()
                    .map(|function| function.name.as_ref())
            })
            .collect::<Vec<_>>();
        // The vDSO's frame has no function, and its caller's is named.
        assert_eq!(functions[0], None, "{context}");
        assert_eq!(functions[1], Some("runtime.nanotime1"), "{context}");
        assert!(functions.len() > 3, "{context}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        let outermost = trace
            .frames
            .last()
            .and_then(|frame| frame.function.as_ref());
        assert_eq!(
            outermost.map(|function| function.role),
            Some(CodeRole::Outermost),
            "{context}"
        );
        scenario.shutdown().await;
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}
