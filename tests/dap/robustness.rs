//! Transports, malformed input, lost events, and abrupt ends.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};
use crate::support::ExternalProcess;

/// Starts `uscope dap --listen` on an unused port and returns its address.
fn listening_adapter() -> (std::process::Child, SocketAddr) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["dap", "--listen", "127.0.0.1:0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the adapter");
    let mut line = String::new();
    BufReader::new(child.stderr.as_mut().expect("stderr"))
        .read_line(&mut line)
        .expect("read the listening address");
    let address = line
        .trim()
        .strip_prefix("uscope dap listening on ")
        .unwrap_or_else(|| panic!("unexpected announcement {line:?}"))
        .parse()
        .expect("an address");
    (child, address)
}

#[test]
fn tcp_serves_one_client_at_a_time_and_refuses_browsers() {
    let (mut adapter, address) = listening_adapter();
    // A web page's request carries Origin; the adapter drops it unanswered.
    let mut browser = TcpStream::connect(address).expect("connect");
    let request = r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#;
    write!(
        browser,
        "Origin: http://example.com\r\nContent-Length: {}\r\n\r\n{request}",
        request.len()
    )
    .expect("write");
    let mut answer = Vec::new();
    browser.read_to_end(&mut answer).expect("read");
    assert!(answer.is_empty(), "{}", String::from_utf8_lossy(&answer));

    let mut first = Dap::connect("tcp first", address);
    let started = first.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    first.event(started.mark, "process", |_| true);
    // A second client waits until the first session ends.
    let mut second = Dap::connect("tcp second", address);
    let initialize = second.send("initialize", json!({"adapterID": "uscope"}));
    first.finish();
    second.success(initialize);
    let launch = second.send("launch", json!({"program": fixture("basic")}));
    second.request("configurationDone", Value::Null);
    second.success(launch);
    assert_eq!(
        second.event(launch.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    second.finish();

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(adapter.id()).expect("pid")),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("signal the adapter");
    assert!(adapter.wait().expect("wait").success());
}

#[test]
fn malformed_messages_are_answered_when_possible_and_never_end_the_session() {
    let mut dap = Dap::start("malformed");
    dap.initialize(Profile::VsCode);
    let missing_type = dap.send_raw(1000, "threads", r#"{"seq":1000,"command":"threads"}"#);
    assert_eq!(dap.failure(missing_type), "the message has no `type`");
    let mark = dap.mark();
    dap.write("{not json");
    assert!(
        dap.output_containing(mark, "important", "ignored a malformed message")
            .contains("not valid JSON")
    );
    assert_eq!(
        dap.request_error(
            "setBreakpoints",
            json!({"source": {"path": "/a.c"}, "breakpoints": "all"})
        ),
        "invalid setBreakpoints arguments at breakpoints: invalid type: string \"all\", expected a sequence"
    );
    assert_eq!(
        dap.request_error("stackTrace", json!({"threadId": "one"})),
        "invalid stackTrace arguments at threadId: invalid type: string \"one\", expected i64"
    );
    dap.finish();
}

#[test]
fn broken_framing_ends_the_session_and_the_program() {
    let mut dap = Dap::start("framing");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    let pid = dap.process_id().expect("process");
    dap.write_bytes(b"Content-Length: lots\r\n\r\n");
    dap.wait_for_exit();
    dap.close_stdin();
    dap.finish();
    let _ = pid;
}

#[test]
fn killing_the_adapter_kills_its_program_and_releases_attached_ones() {
    let mut launched = Dap::start("killed launched");
    let started = launched.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    launched.event(started.mark, "process", |_| true);
    let pid = launched.process_id().expect("process");
    launched.signal(nix::sys::signal::Signal::SIGKILL);
    launched.abandon();
    assert!(
        wait_until_gone(pid),
        "the launched program outlived its adapter"
    );

    let mut process = ExternalProcess::spawn(&fixture("attach"));
    let mut attached = Dap::start("killed attached");
    let started = attached.begin(
        Profile::VsCode,
        (
            "attach",
            json!({"pid": process.process_id().get(), "stopOnEntry": true}),
        ),
        &Configuration::default(),
    );
    attached.stopped(started.mark);
    attached.signal(nix::sys::signal::Signal::SIGKILL);
    attached.abandon();
    // The kernel detaches a dead tracer's tracees; this one was stopped,
    // and runs on once released.
    process.release();
    let status = process.wait();
    assert_eq!(status.code(), Some(23), "{status:?}");
}

/// Waits for a process to disappear, since nothing reports the death of
/// a process this test did not start.
fn wait_until_gone(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Err(_) => return true,
            Ok(status) if status.contains("State:\tZ") => return true,
            Ok(_) => std::thread::yield_now(),
        }
    }
    false
}

#[test]
fn missed_events_are_recovered_from_the_debuggers_state() {
    // With room for one event, the adapter misses most of a burst of
    // thread starts and exits, and must catch up from the debugger's state.
    let mut dap = Dap::start_in("lagged", &[], &[("USCOPE_EVENT_CAPACITY", "1")]);
    let started = dap.launch(
        Profile::Helix,
        &fixture("thread-stress"),
        json!({}),
        &Configuration::default(),
    );
    assert_eq!(
        dap.event(started.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.event(started.mark, "terminated", |_| true);
    dap.finish();
}
