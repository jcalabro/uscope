use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn assert_success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "uscope failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

#[test]
fn batch_mode_executes_a_command_file() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "-c"])
        .arg(fixture("tests/fixtures/basic.uscope"))
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert_eq!(stdout.matches("stopped at breakpoint").count(), 2);
    assert!(stdout.contains("breakpoint_target at"));
    assert!(stdout.contains("basic.c:"));
    assert!(stdout.contains("inferior exited with status 0"));
}

#[test]
fn batch_mode_streams_commands_from_stdin() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg("--batch")
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"break breakpoint_target\nrun\nquit\n")
        .expect("write commands");
    let stdout = assert_success(child.wait_with_output().expect("wait for uscope"));

    assert!(stdout.contains("breakpoint set"));
    assert!(stdout.contains("stopped at breakpoint"));
}

#[test]
fn batch_mode_reports_command_context() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "invalid"])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error output");

    assert!(!output.status.success());
    assert!(stderr.contains("--eval #1"));
    assert!(stderr.contains("invalid command: invalid"));
}

#[test]
fn batch_mode_prints_a_nested_backtrace() {
    let executable = fixture("build/test-programs/unwind-o2");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break deepest",
            "--eval",
            "run",
            "--eval",
            "backtrace",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);
    let deepest = stdout.find("in deepest").expect("deepest frame");
    let middle = stdout.find("in middle").expect("middle frame");
    let outer = stdout.find("in outer").expect("outer frame");
    let main = stdout.find("in main").expect("main frame");

    assert!(
        deepest < middle && middle < outer && outer < main,
        "{stdout}"
    );
    assert!(stdout.contains("unwind stopped:"));
}

#[test]
fn batch_mode_prints_registers_one_per_line() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
            "--eval",
            "registers",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.lines().any(|line| line.starts_with("rax ")));
    assert!(stdout.lines().any(|line| line.starts_with("rsp ")));
    assert!(stdout.lines().any(|line| line.starts_with("rip ")));
}

#[test]
fn plain_repl_preserves_output_and_exits_at_eof() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"break breakpoint_target\nrun\nregisters\n")
        .expect("write commands");
    let stdout = assert_success(child.wait_with_output().expect("wait for uscope"));

    assert!(stdout.starts_with("debugging "));
    assert!(stdout.lines().any(|line| line.starts_with("rax ")));
    assert!(stdout.lines().any(|line| line.starts_with("orig_rax ")));
}

#[test]
fn plain_repl_reports_errors_and_continues() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"invalid\nquit\n")
        .expect("write commands");
    let output = child.wait_with_output().expect("wait for uscope");
    let stderr = String::from_utf8(output.stderr.clone()).expect("UTF-8 error output");

    assert_success(output);
    assert!(stderr.contains("repl:1"));
    assert!(stderr.contains("invalid command: invalid"));
}

#[test]
fn ctrl_c_shuts_down_and_reaps_a_running_inferior() {
    let executable = fixture("build/test-programs/spin");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"run\n")
        .expect("write command");

    let debugger_pid = child.id();
    let inferior_pid = wait_for_child_process(debugger_pid).unwrap_or_else(|| {
        child.kill().expect("kill debugger after test timeout");
        child.wait().expect("reap debugger after test timeout");
        panic!("debugger did not launch an inferior");
    });

    kill(
        Pid::from_raw(i32::try_from(debugger_pid).expect("debugger PID fits i32")),
        Signal::SIGINT,
    )
    .expect("interrupt uscope");

    let output = child.wait_with_output().expect("wait for uscope");

    assert_success(output);
    assert!(
        !PathBuf::from(format!("/proc/{inferior_pid}")).exists(),
        "inferior {inferior_pid} survived debugger shutdown"
    );
}

fn wait_for_child_process(parent: u32) -> Option<u32> {
    let tasks = PathBuf::from(format!("/proc/{parent}/task"));
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        for task in fs::read_dir(&tasks).expect("read debugger tasks") {
            let children = task.expect("read debugger task").path().join("children");
            let contents = fs::read_to_string(children).expect("read debugger children");

            if let Some(pid) = contents.split_whitespace().next() {
                return Some(pid.parse().expect("numeric inferior PID"));
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
