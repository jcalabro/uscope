use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
