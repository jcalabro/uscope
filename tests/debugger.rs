use std::path::PathBuf;
use uscope::{BreakpointSpec, Debugger, StopReason};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/basic")
}

#[test]
fn breakpoint_is_reinserted_and_inferior_memory_can_be_read() {
    let fixture = fixture();
    assert!(
        fixture.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut debugger = Debugger::new(&fixture).expect("load debugger");
    debugger
        .add_breakpoint(BreakpointSpec::Function("breakpoint_target".into()))
        .expect("set breakpoint");

    let first = debugger.run().expect("run to first breakpoint");
    let first_address = match first {
        StopReason::Breakpoint { address } => address,
        other => panic!("expected breakpoint, got {other:?}"),
    };
    let value_address = debugger
        .runtime_address("uscope_value")
        .expect("resolve global");
    assert_eq!(
        debugger
            .read_word(value_address)
            .expect("read inferior memory"),
        0x1122_3344_5566_7788
    );

    assert_eq!(
        debugger.resume().expect("run to second breakpoint"),
        StopReason::Breakpoint {
            address: first_address
        }
    );
    assert_eq!(
        debugger.resume().expect("finish inferior"),
        StopReason::Exited(0)
    );
    debugger.shutdown().expect("shutdown worker");
}
