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

fn assert_no_sgr(output: &str) {
    assert!(
        !output.contains("\x1b["),
        "unexpected terminal styling in {output:?}"
    );
}

#[test]
fn redirected_and_batch_output_is_plain_unless_color_is_forced() {
    let executable = fixture("build/test-programs/basic");

    let automatic = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "help"])
        .arg(&executable)
        .output()
        .expect("run uscope with automatic color");
    assert_no_sgr(&assert_success(automatic));

    let forced = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--color", "always", "--eval", "help"])
        .arg(&executable)
        .output()
        .expect("run uscope with forced color");
    let stdout = assert_success(forced);
    assert!(stdout.contains("\x1b["), "missing forced color: {stdout:?}");

    let disabled = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .env("CLICOLOR_FORCE", "1")
        .args(["--batch", "--color", "never", "--eval", "help"])
        .arg(executable)
        .output()
        .expect("run uscope with color disabled");
    assert_no_sgr(&assert_success(disabled));
}

#[test]
fn forced_color_styles_both_stdout_and_stderr() {
    let executable = fixture("build/test-programs/basic");
    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--color", "always"])
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
    let stdout = String::from_utf8(output.stdout.clone()).expect("UTF-8 output");
    let stderr = String::from_utf8(output.stderr.clone()).expect("UTF-8 error output");

    assert_success(output);
    assert!(
        stdout.contains("\x1b["),
        "stdout was not colored: {stdout:?}"
    );
    assert!(
        stderr.contains("\x1b["),
        "stderr was not colored: {stderr:?}"
    );
    assert!(stderr.contains("error"), "{stderr:?}");
}

#[test]
fn color_styles_source_metadata_but_never_source_text() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--color",
            "always",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);
    let current = stdout
        .lines()
        .find(|line| line.contains("uint64_t breakpoint_target(void)"))
        .expect("current source line");
    let (_, source) = current.split_once("| ").expect("source separator");

    assert!(
        current.contains("\x1b["),
        "metadata was not styled: {current:?}"
    );
    assert!(
        !source.contains("\x1b["),
        "source text was styled: {current:?}"
    );
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
fn batch_mode_prints_every_location_of_an_inline_breakpoint() {
    let executable = fixture("build/test-programs/inline-gcc-o2");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "break leaf"])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout.contains("breakpoint 1 set at 6 locations"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("  image address ").count(), 6, "{stdout}");
}

#[test]
fn batch_mode_sets_lists_and_deletes_source_and_file_function_breakpoints() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break basic.c:11",
            "--eval",
            "break basic.c:breakpoint_target",
            "--eval",
            "breakpoints",
            "--eval",
            "delete 1",
            "--eval",
            "clear all",
            "--eval",
            "info breakpoints",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("1  basic.c:11  1 location"), "{stdout}");
    assert!(
        stdout.contains("2  basic.c:breakpoint_target  1 location"),
        "{stdout}"
    );
    assert!(stdout.contains("deleted breakpoint 1"), "{stdout}");
    assert!(stdout.contains("deleted 1 breakpoint"), "{stdout}");
    assert!(stdout.ends_with("no breakpoints\n"), "{stdout}");
}

#[test]
fn help_and_cls_are_generated_without_changing_clear_semantics() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help",
            "--eval",
            "help clear",
            "--eval",
            "cls",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("help [command]"), "{stdout}");
    assert!(
        stdout.contains("break <function|address|file:line|file:function> (b)"),
        "{stdout}"
    );
    assert!(stdout.contains("continue (c)"), "{stdout}");
    assert!(stdout.contains("help [command] (?)"), "{stdout}");
    assert!(stdout.contains("delete <id|all>"), "{stdout}");
    assert!(stdout.contains("aliases: clear"), "{stdout}");
    assert!(stdout.ends_with("\x1b[2J\x1b[H"), "{stdout:?}");
}

#[test]
fn print_and_p_render_stack_scalars_and_generated_alias_help() {
    let executable = fixture("build/test-programs/variables-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help p",
            "--eval",
            "help pause",
            "--eval",
            "break variables.c:52",
            "--eval",
            "run",
            "--eval",
            "p signed_int",
            "--eval",
            "print",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("print [variable]\n"), "{stdout}");
    assert!(stdout.contains("aliases: p"), "{stdout}");
    assert!(stdout.contains("pause\n  Pause execution"), "{stdout}");
    assert!(
        !stdout.contains("pause\n  Pause execution\n  aliases:"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("(int) signed_int = -1234567").count(), 2);
    assert!(stdout.contains("(char) character = 65 'A'"), "{stdout}");
    assert!(stdout.contains("(float) single = 1.25"), "{stdout}");
    assert!(
        stdout.contains("(double) double_precision = -2.5"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(long double) extended = 3.125"),
        "{stdout}"
    );
}

#[test]
fn print_and_p_render_parameters_and_locals() {
    let executable = fixture("build/test-programs/variables-parameters-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help print",
            "--eval",
            "break variables-parameters.c:22",
            "--eval",
            "run",
            "--eval",
            "p signed_int",
            "--eval",
            "print",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout.contains("Print one or all visible variables"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("(int) signed_int = -1234567").count(), 2);
    assert!(
        stdout.contains("(long double) extended = 3.125"),
        "{stdout}"
    );
    assert!(stdout.contains("(int) local = 99"), "{stdout}");
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
fn batch_mode_lists_threads_and_steps_one_instruction() {
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
            "threads",
            "--eval",
            "stepi",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("* ") && line.contains(" stopped")),
        "{stdout}"
    );
    assert!(stdout.contains("stopped after instruction step"));
}

#[test]
fn breakpoint_stops_print_source_context_from_any_working_directory() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .current_dir("/")
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("tests/fixtures/basic.c:5"));
    assert!(stdout.contains("=> 5 | __attribute__((noinline)) uint64_t breakpoint_target(void) {"));
    assert!(stdout.contains("   8 |"));
}

#[test]
fn list_command_prints_the_current_source_context() {
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
            "l",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert_eq!(stdout.matches("tests/fixtures/basic.c:5").count(), 2);
    assert_eq!(
        stdout
            .matches("=> 5 | __attribute__((noinline)) uint64_t breakpoint_target(void) {")
            .count(),
        2
    );
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

#[test]
fn ctrl_c_pauses_a_running_inferior_before_accepting_more_commands() {
    let executable = fixture("build/test-programs/spin");
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
    let mut stdin = child.stdin.take().expect("stdin pipe");
    stdin.write_all(b"run\n").expect("write run command");

    let debugger_pid = child.id();
    let inferior_pid = wait_for_child_process(debugger_pid).expect("debugger launched inferior");
    wait_for_running_process(inferior_pid);
    kill(
        Pid::from_raw(i32::try_from(debugger_pid).expect("debugger PID fits i32")),
        Signal::SIGINT,
    )
    .expect("pause uscope");
    stdin
        .write_all(b"registers\nquit\n")
        .expect("write inspection commands");
    drop(stdin);

    let stdout = assert_success(child.wait_with_output().expect("wait for uscope"));
    assert!(stdout.contains("inferior paused"), "{stdout}");
    assert!(stdout.lines().any(|line| line.starts_with("rip ")));
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

fn wait_for_running_process(pid: u32) {
    let status = PathBuf::from(format!("/proc/{pid}/status"));
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        let contents = fs::read_to_string(&status).expect("read inferior status");
        let stopped = contents
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .is_some_and(|state| state.trim_start().starts_with(['T', 't']));
        if !stopped {
            return;
        }
        assert!(Instant::now() < deadline, "inferior did not start running");
        thread::sleep(Duration::from_millis(10));
    }
}
