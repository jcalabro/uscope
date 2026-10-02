//! Changing values and memory at a stop.

use super::*;

/// Stops `fixture` in `function` at its first statement after `marker`'s
/// line, as a source breakpoint there does.
async fn stopped_at_line(fixture: &str, source: &str, marker: &str) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/c")
        .join(source);
    let line = std::fs::read_to_string(&path)
        .expect("source")
        .lines()
        .position(|line| line.contains(marker))
        .expect("marker") as u64
        + 1;
    scenario.add_source_breakpoint(source, line).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario
}

fn signed(value: &uscope::InspectedValue) -> i128 {
    match available_value(&value.state) {
        uscope::VariableValue::Scalar(ScalarValue::Signed(value))
        | uscope::VariableValue::Enumeration {
            value: uscope::IntegerValue::Signed(value),
            ..
        } => *value,
        other => panic!("not a signed value: {other:?}"),
    }
}

fn path(text: &str) -> uscope::ValueExpression {
    uscope::parse_value_expression(text)
        .expect("expression")
        .expression
}

/// A view of a frame of the stopped thread.
async fn frame_view(scenario: &mut Scenario, level: u32) -> uscope::StopContext {
    let snapshot = scenario.snapshot().await;
    let uscope::InferiorState::Stopped {
        stop_id, thread_id, ..
    } = snapshot.inferior
    else {
        panic!("not stopped");
    };
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    uscope::StopContext {
        stop: stop_id,
        thread: thread_id,
        frame: trace.frames[usize::try_from(level).expect("level")].id,
    }
}

#[tokio::test]
async fn assignments_change_memory_the_program_then_uses() {
    let mut scenario = stopped_at_line(
        "variables-gcc-o0",
        "variables.c",
        "return **pointer_pointer",
    )
    .await;
    let innermost = frame_view(&mut scenario, 0).await;
    let handle = scenario.handle().clone();
    let view = handle.at(innermost);
    let assigned = scenario
        .operation("assign", view.assign(path("pointee"), "100"))
        .await;
    assert_eq!(signed(&assigned), 100);
    // The pointer sees the new value: the write went to memory.
    let through = scenario
        .operation("inspect", view.inspect(path("*pointer")))
        .await;
    assert_eq!(signed(&through), 100);
    let doubled = scenario
        .operation(
            "assign expression",
            view.assign(path("parameter"), "parameter * 2 + 1"),
        )
        .await;
    assert_eq!(signed(&doubled), 81);
    for (target, value, reason) in [
        (
            "pair",
            "1",
            "only numbers, booleans, enumerations, and pointers can be assigned",
        ),
        (
            "pointee",
            "1.5",
            "a floating-point value cannot be stored in an integer; write an integer",
        ),
        (
            "pointee",
            "no_such_value",
            "no visible variable or parameter named 'no_such_value' was found",
        ),
    ] {
        let error = scenario
            .attempt("refused assignment", view.assign(path(target), value))
            .await;
        assert_eq!(
            error.map(|_| ()).map_err(|error| error.to_string()),
            Err(format!(
                "cannot assign to {}: {reason}",
                if reason.contains("no visible") {
                    value
                } else {
                    target
                }
            )),
            "{target} = {value}"
        );
    }
    let caller = handle.at(frame_view(&mut scenario, 1).await);
    let error = scenario
        .attempt(
            "narrow assignment",
            caller.assign(path("unsigned_character"), "300"),
        )
        .await;
    assert_eq!(
        error.map(|_| ()).map_err(|error| error.to_string()),
        Err(
            "cannot assign to unsigned_character: 300 does not fit a unsigned 8-bit value"
                .to_owned()
        )
    );
    // A caller's variable in memory can change too; main checks it.
    let changed = scenario
        .operation("assign in caller", caller.assign(path("signed_int"), "5"))
        .await;
    assert_eq!(signed(&changed), 5);
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn enumerations_take_their_enumerators_names() {
    let mut scenario = Scenario::launch("enums-c-gcc-o0");
    scenario.add_breakpoint("inspect_enums").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = frame_view(&mut scenario, 1).await;
    let handle = scenario.handle().clone();
    let caller = handle.at(context);
    let assigned = scenario
        .operation(
            "assign enumerator",
            caller.assign(path("signed_value"), "SIGNED_ZERO"),
        )
        .await;
    assert_eq!(signed(&assigned), 0);
    // inspect_enums now sees a different value than main set.
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn registers_hold_whole_variables_of_the_innermost_frame_only() {
    let mut scenario = Scenario::launch("variables-gcc-o2");
    scenario.add_breakpoint("pointer_target").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = frame_view(&mut scenario, 0).await;
    let handle = scenario.handle().clone();
    let innermost = handle.at(context);
    let current = scenario
        .operation("inspect", innermost.inspect(path("parameter")))
        .await;
    assert!(
        matches!(
            current.state,
            VariableState::Available {
                source: uscope::VariableValueSource::Register(_),
                ..
            }
        ),
        "the optimized parameter lives in a register: {current:?}"
    );
    let assigned = scenario
        .operation("assign register", innermost.assign(path("parameter"), "41"))
        .await;
    assert_eq!(signed(&assigned), 41);
    // The optimized caller's variables are constants with no storage.
    let context = frame_view(&mut scenario, 1).await;
    let caller = handle.at(context);
    let refused = scenario
        .attempt("assign constant", caller.assign(path("signed_int"), "1"))
        .await;
    assert_eq!(
        refused.map(|_| ()).map_err(|error| error.to_string()),
        Err(
            "cannot assign to signed_int: the debug information computes it; it has no storage"
                .to_owned()
        )
    );
    // The function computes with the register's new value.
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn memory_writes_keep_breakpoints_installed_and_span_words() {
    let mut scenario = Scenario::launch("basic");
    let breakpoint = scenario.add_breakpoint("breakpoint_target").await;
    let reason = scenario.run_to_stop().await;
    let address = crate::support::breakpoint_address(&reason);
    let handle = scenario.handle().clone();
    // The program's bytes at the breakpoint, written back over the trap.
    let code = scenario
        .operation("read code", handle.read_memory(address, 16))
        .await;
    let written = scenario
        .operation("write code", handle.write_memory(address, &code.bytes))
        .await;
    assert_eq!(written, 16);
    let again = scenario
        .operation("read code again", handle.read_memory(address, 16))
        .await;
    assert_eq!(again.bytes, code.bytes, "traps stay hidden");
    // The second call still stops at the breakpoint.
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { hits, .. } if hits[0].breakpoint == breakpoint.id
    ));
    // Bytes across a word boundary, starting mid-word.
    let global = scenario
        .operation("address", handle.runtime_address("uscope_value"))
        .await;
    let start = VirtualAddress::new(global.get() + 3);
    scenario
        .operation("write across words", handle.write_memory(start, &[0xaa; 5]))
        .await;
    let value = scenario
        .operation("read value", handle.read_memory(global, 8))
        .await;
    assert_eq!(
        &*value.bytes,
        &[0x88, 0x77, 0x66, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa]
    );
    let unwritable = scenario
        .attempt(
            "write nowhere",
            handle.write_memory(VirtualAddress::new(8), &[1]),
        )
        .await;
    assert_eq!(
        unwritable.map_err(|error| error.to_string()),
        Err("memory at 0x8 cannot be written".to_owned())
    );
    scenario.shutdown().await;
}
