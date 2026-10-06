mod support;

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use object::{Object, ObjectSection, ObjectSymbol};

const BASIC: &str = "build/test-programs/basic";
const SEGV_CORE: &str = "build/test-programs/crash-gcc-o0-segv.core";
const FRAMES_CORE: &str = "build/test-programs/frames-gcc-o0.core";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// Runs uscope in the repository with no standard input.
fn uscope(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(arguments)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run uscope")
}

/// Runs `commands` in batch mode after the other `arguments`.
fn batch_output(arguments: &[&str], commands: &[&str]) -> Output {
    let mut all = vec!["--batch"];
    for command in commands {
        all.extend(["--eval", command]);
    }
    all.extend_from_slice(arguments);
    uscope(&all)
}

/// Runs `commands` in batch mode and returns the output of the session,
/// which must succeed.
fn batch(arguments: &[&str], commands: &[&str]) -> String {
    assert_success(batch_output(arguments, commands))
}

/// Runs `commands` through uscope's standard input, which reports each
/// failed command and carries on, and returns stdout and stderr.
fn piped(arguments: &[&str], commands: &[&str]) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(arguments)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start uscope");
    let mut stdin = child.stdin.take().expect("uscope's stdin");
    for command in commands {
        writeln!(stdin, "{command}").expect("write a command");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("wait for uscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (assert_success(output), stderr)
}

fn assert_success(output: Output) -> String {
    assert!(
        output.status.success(),
        "uscope failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

fn assert_failure(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(expected),
        "expected failure containing {expected:?}:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn assert_no_sgr(output: &str) {
    assert!(
        !output.contains("\x1b["),
        "unexpected terminal styling in {output:?}"
    );
}

/// Asserts each expected text appears, in order, after the previous one.
fn assert_in_order(output: &str, expected: &[&str]) {
    let mut rest = output;
    for text in expected {
        let found = rest
            .find(text)
            .unwrap_or_else(|| panic!("{text:?} does not follow in:\n{output}"));
        rest = &rest[found + text.len()..];
    }
}

/// Returns the index of the first output line at or after `from` that
/// satisfies `predicate`.
fn line_index(lines: &[&str], from: usize, predicate: impl Fn(&str) -> bool) -> usize {
    lines[from..]
        .iter()
        .position(|line| predicate(line))
        .map_or_else(
            || panic!("no matching line after {from} in:\n{}", lines.join("\n")),
            |index| from + index,
        )
}

fn symbol_address(executable: &std::path::Path, name: &str) -> u64 {
    let data = fs::read(executable).expect("read fixture");
    let object = object::File::parse(data.as_slice()).expect("parse fixture");
    object
        .symbols()
        .find_map(|symbol| {
            (symbol.name().ok() == Some(name) && symbol.is_definition()).then(|| symbol.address())
        })
        .unwrap_or_else(|| panic!("fixture has no defined symbol named {name:?}"))
}

fn frames_line(needle: &str) -> usize {
    fs::read_to_string(fixture("tests/fixtures/c/frames.c"))
        .expect("read frames fixture")
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("frames.c has no line containing {needle:?}"))
        + 1
}

/// Returns the state letter of a live process, or `None` once it is gone.
fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain anything, so the state follows its last
    // closing parenthesis.
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// The harness's deadline for anything a test waits to observe.
const DEADLINE: Duration = Duration::from_secs(5);

/// A uscope process whose output a test reads as it arrives. It is killed if
/// the test ends first, which also kills an inferior it launched or attached
/// to, since uscope traces with `PTRACE_O_EXITKILL`.
struct Uscope {
    child: Child,
    stdout: mpsc::Receiver<String>,
    transcript: String,
    stderr: Option<thread::JoinHandle<String>>,
}

impl Uscope {
    /// Starts uscope with piped standard streams.
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run uscope");
        let stdout = child.stdout.take().expect("stdout pipe");
        let mut stderr = child.stderr.take().expect("stderr pipe");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Self {
            child,
            stdout: receiver,
            transcript: String::new(),
            stderr: Some(stderr),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, text: &str) {
        self.child
            .stdin
            .as_mut()
            .expect("uscope stdin")
            .write_all(text.as_bytes())
            .expect("write to uscope");
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Reads standard output until a line satisfies `predicate`.
    fn line(&mut self, what: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let line = self
                .stdout
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "uscope never printed {what} ({error}):\n{}",
                        self.transcript
                    )
                });
            self.transcript.push_str(&line);
            self.transcript.push('\n');
            if predicate(&line) {
                return line;
            }
        }
    }

    /// Waits for uscope to exit and returns everything it printed.
    fn finish(mut self) -> std::process::Output {
        let mut status = None;
        support::wait_until("uscope exits", || {
            status = self.child.try_wait().expect("poll uscope");
            status.is_some()
        });
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self
                .stdout
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => {
                    self.transcript.push_str(&line);
                    self.transcript.push('\n');
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("uscope's output never closed:\n{}", self.transcript)
                }
            }
        }
        let stderr = self
            .stderr
            .take()
            .expect("stderr reader")
            .join()
            .expect("read uscope stderr");
        std::process::Output {
            status: status.expect("exit status"),
            stdout: std::mem::take(&mut self.transcript).into_bytes(),
            stderr: stderr.into_bytes(),
        }
    }
}

impl Drop for Uscope {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn help_is_task_oriented_and_progressive() {
    let short = assert_success(uscope(&["-h"]));
    assert_in_order(
        &short,
        &[
            "Debug Linux x86-64 programs, processes, and core dumps",
            "Usage:",
            "Tools:",
            "Target:",
            "Startup:",
            "Common forms:",
        ],
    );
    for invocation in [
        "uscope EXECUTABLE [-- ARGS...]",
        "uscope --attach PID [EXECUTABLE]",
        "uscope --core CORE [EXECUTABLE]",
    ] {
        assert!(short.contains(invocation), "{short}");
    }
    for advanced in ["--sysroot", "--module-path", "--source-map"] {
        assert!(!short.contains(advanced), "{short}");
    }

    let long = assert_success(uscope(&["--help"]));
    assert_in_order(
        &long,
        &[
            "Tools:",
            "Target:",
            "Startup:",
            "Core dump files:",
            "Debug information:",
            "Launch environment:",
            "Display:",
        ],
    );
    for advanced in ["--sysroot", "--module-path", "--source-map"] {
        assert!(long.contains(advanced), "{long}");
    }
    assert!(!long.contains("uscope dap ["), "{long}");

    for subcommand in ["dap", "views"] {
        assert!(short.contains(subcommand), "{short}");
        let own = assert_success(uscope(&[subcommand, "--help"]));
        assert!(own.contains(&format!("uscope {subcommand}")), "{own}");
    }
}

#[test]
fn an_empty_command_prints_short_help() {
    let empty = uscope(&[]);
    let short_help = uscope(&["-h"]);

    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    assert_eq!(empty.stdout, short_help.stdout);
    assert_eq!(empty.stderr, short_help.stderr);
}

#[test]
fn command_line_errors_explain_themselves() {
    let missing_process = i32::MAX.to_string();
    for (arguments, expected) in [
        (
            &["--attach", &missing_process, "--batch"][..],
            "pass EXECUTABLE explicitly",
        ),
        (
            &["--core", SEGV_CORE, "--attach", "1"],
            "cannot be used with",
        ),
        (
            &["--core", SEGV_CORE, "--cwd", "/", "--batch"],
            "cannot be used with",
        ),
        (&["--allow-module-mismatch", BASIC], "--core <CORE>"),
        (&["--sysroot", "/", BASIC], "--core <CORE>"),
        (&["--module-path", "/", BASIC], "--core <CORE>"),
        (
            &["--env", "novalue", BASIC],
            "expected NAME=VALUE, found 'novalue'",
        ),
        (
            &[BASIC, "--source-map", "/nonexistent"],
            "2 values required for '--source-map <FROM> <TO>'",
        ),
        (
            &[
                "--core",
                "build/test-programs/core-foreign/crash.core",
                "--module-path",
                "absent",
                "--batch",
            ],
            "cannot search absent for core dump modules: No such file or directory",
        ),
        (
            &[
                "--core",
                "build/test-programs/core-missing-executable/crash.core",
                "--batch",
            ],
            "no longer exists; supply the executable explicitly",
        ),
        (
            &["--core", "build/test-programs/crash-gcc-o0", "--batch"],
            "invalid core dump: the ELF file is not a core dump",
        ),
        (
            &[
                "--core",
                SEGV_CORE,
                "build/test-programs/crash-gcc-o0-rebuilt",
                "--batch",
            ],
            "does not match the image recorded in the core dump: the build-id note differs; allow module mismatches",
        ),
    ] {
        assert_failure(&uscope(arguments), expected);
    }
}

#[test]
fn pid_only_attach_discovers_the_executable_and_quit_detaches() {
    let mut target = support::ExternalProcess::spawn(&fixture("build/test-programs/attach"));
    let process = target.process_id().get().to_string();
    batch(&["--attach", &process], &["quit"]);
    target.release();
    assert_eq!(target.wait().code(), Some(23));
}

#[test]
fn color_follows_the_choice_and_the_environment_on_both_streams() {
    // Redirected output is plain unless color is forced, and `never` wins
    // over the environment.
    assert_no_sgr(&batch(&[BASIC], &["help"]));
    let colored_help = assert_success(uscope(&["--color", "always", "-h"]));
    assert!(colored_help.contains("\x1b["), "{colored_help:?}");
    let plain_help = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .env("CLICOLOR_FORCE", "1")
        .args(["--color", "never", "-h"])
        .output()
        .expect("run uscope help with color disabled");
    assert_no_sgr(&assert_success(plain_help));

    let never = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .env("CLICOLOR_FORCE", "1")
        .args(["--batch", "--color", "never", "--eval", "help"])
        .arg(fixture(BASIC))
        .output()
        .expect("run uscope with color disabled");
    assert_no_sgr(&assert_success(never));

    let forced = batch_output(
        &["--color", "always", BASIC],
        &["help", "break breakpoint_target", "run", "invalid"],
    );
    let stdout = String::from_utf8_lossy(&forced.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&forced.stderr).into_owned();
    assert!(!forced.status.success(), "{stdout}");
    // Help styles command names and aliases, and nothing else.
    assert!(stdout.contains("\x1b[1m\x1b[94mbreak\x1b[0m"), "{stdout:?}");
    assert!(stdout.contains("\x1b[94mb\x1b[0m"), "{stdout:?}");
    assert!(
        !stdout.contains("\x1b[94m\x1b[0m"),
        "empty aliases emitted styling: {stdout:?}"
    );
    for plain in ["commands", "Set a breakpoint", "Use `help <command>`"] {
        assert!(!stdout.contains(&format!("\x1b[2m{plain}")), "{stdout:?}");
    }
    // Source metadata is styled, but never the source text.
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
    // A failed command ends the batch, naming the command.
    assert!(
        stderr.contains("\x1b[1m\x1b[91merror\x1b[0m: --eval #4: unknown command 'invalid'"),
        "{stderr:?}"
    );
}

#[test]
fn batch_mode_executes_a_command_file() {
    let stdout = assert_success(uscope(&[
        "--batch",
        "-c",
        "tests/fixtures/basic.uscope",
        BASIC,
    ]));
    assert_eq!(stdout.matches("stopped at breakpoint").count(), 2);
    assert!(stdout.contains("breakpoint_target at"));
    assert!(stdout.contains("basic.c:"));
    assert!(stdout.contains("inferior exited with status 0"));
}

#[test]
fn batch_mode_streams_commands_from_stdin() {
    let (stdout, _) = piped(
        &["--batch", BASIC],
        &["break breakpoint_target", "run", "quit"],
    );
    assert!(stdout.contains("breakpoint 1 set"));
    assert!(stdout.contains("stopped at breakpoint"));
}

#[test]
fn piped_input_reports_errors_and_runs_to_its_end() {
    let (stdout, stderr) = piped(
        &[BASIC],
        &["invalid", "break breakpoint_target", "run", "registers"],
    );
    assert!(
        stderr.contains("stdin:1: unknown command 'invalid'"),
        "{stderr}"
    );
    assert!(stdout.starts_with("debugging "), "{stdout}");
    assert!(stdout.lines().any(|line| line.starts_with("rax ")));
    assert!(stdout.lines().any(|line| line.starts_with("orig_rax ")));
}

#[test]
fn command_errors_show_the_usage_or_the_reason() {
    let (stdout, stderr) = piped(
        &[BASIC],
        &[
            "run ignored",
            "break",
            // Only `info symbol` and `info view` take an argument, and they
            // require one.
            "info symbol",
            "info view",
            "info breakpoints 0x10",
            "info core",
            "handle SIGNOPE nostop",
            "handle SIGUSR1 sometimes",
            "handle",
            "clear",
            "cls",
        ],
    );
    let info = "usage: info breakpoints|watchpoints|signals|core|symbol|view [argument...]";
    let clear = "cannot clear screen: stdout is not an ANSI terminal";
    assert_in_order(
        &stderr,
        &[
            "usage: run\n",
            "usage: break <function|0xaddress|file:line|file:function>",
            info,
            info,
            info,
            "no core dump is open",
            "unknown signal 'SIGNOPE'",
            "unknown signal action 'sometimes'",
            "usage: handle <signal> [action] [action] [action]",
            clear,
            clear,
        ],
    );
    assert!(!stdout.contains("\x1b[2J\x1b[H"), "{stdout:?}");
}

#[test]
fn help_lists_every_command_and_details_one_by_name_or_alias() {
    let stdout = batch(
        &[BASIC],
        &[
            "help",
            "help clear",
            "help del",
            "help fin",
            "help break",
            "help where",
            "help continue",
        ],
    );
    for expected in [
        "  break        b       Set a breakpoint",
        "  finish       fin, f  Run until the selected frame returns",
        "  continue     c       Continue execution",
        "  delete       del, d  Delete logical breakpoints",
        "  clear        cls     Clear and redraw the terminal",
        "  help         h, ?    Show command help",
        "  Clear and redraw the terminal\n  aliases: cls",
        "delete <id|all>",
        "aliases: del, d",
        "  Run until the selected frame returns to its caller\n  aliases: fin, f",
        "  Set a breakpoint, optionally stopping only at hits such as >=5, ==3, or %10\n  aliases: b\n  usage: break <function|0xaddress|file:line|file:function> [hit-condition]",
        "  Show the selected frame's execution location",
        "  Continue execution\n  aliases: c",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }
    let (overview, _) = stdout
        .split_once("Use `help <command>` for aliases and usage.")
        .expect("the overview's last line");
    assert!(!overview.contains("<function"), "{overview}");
    for absent in [
        "break\n  Set a breakpoint",
        "finish\n  Run until the selected frame returns",
        "usage: where",
        "  Show the selected frame's execution location\n  aliases:",
        "usage: continue",
    ] {
        assert!(!stdout.contains(absent), "{absent:?} in:\n{stdout}");
    }
}

#[test]
fn batch_mode_prints_every_location_of_an_inline_breakpoint() {
    let stdout = batch(&["build/test-programs/inline-gcc-o2"], &["break leaf"]);
    assert!(
        stdout.contains("breakpoint 1 set at 6 locations"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("  image address ").count(), 6, "{stdout}");
}

#[test]
fn library_breakpoints_resolve_at_runtime_and_frames_show_their_own_sources() {
    let stdout = batch(
        &["build/test-programs/module-frames-gcc-o0"],
        &[
            "break main",
            "run",
            "break dso_apply",
            "continue",
            "break module_callback",
            "continue",
            "backtrace",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 2 set at virtual address 0x",
            "stopped at breakpoint 2 (hit 1)",
            "module-frames/library.c:5",
            "stopped at breakpoint 3 (hit 1)",
            "#0  0x",
            " in module_callback at ",
            "#1  0x",
            " in dso_apply at ",
            "tests/fixtures/c/module-frames/library.c:6\n",
            "#2  0x",
            " in main at ",
        ],
    );
}

#[test]
fn batch_mode_sets_lists_and_deletes_source_and_file_function_breakpoints() {
    let stdout = batch(
        &[BASIC],
        &[
            "break basic.c:11",
            "break basic.c:breakpoint_target",
            "breakpoints",
            "del 1",
            "d all",
            "info breakpoints",
        ],
    );
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
fn batch_mode_sets_skips_and_amends_breakpoint_hit_conditions() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted ==3",
            "break shared %4",
            "run",
            "breakpoints",
            "continue",
            "ignore 1 5",
            "hits 2 always",
            "info breakpoints",
            "delete 2",
            "continue",
            "hits 1 ==2",
            "continue",
            "breakpoints",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 set at image address ",
            ", stops at hits ==3\n",
            "breakpoint 2 set at 2 locations, stops at hits %4\n",
            // The fourth hit is the second inline site of the second call.
            "stopped at breakpoint 2 (hit 4) at ",
            "1  counted  1 location  hit 2 times  stops at hits ==3\n",
            "2  shared  2 locations  hit 4 times  stops at hits %4\n",
            "stopped at breakpoint 1 (hit 3) at ",
            "breakpoint 1 ignores its next 5 hits\n",
            "breakpoint 2 stops at every hit, hit 4 times so far\n",
            "1  counted  1 location  hit 3 times  stops at hits >=9\n",
            "2  shared  2 locations  hit 4 times\n",
            "deleted breakpoint 2\n",
            "stopped at breakpoint 1 (hit 9) at ",
            "breakpoint 1 stops at hits ==2 (no later hit can stop), hit 9 times so far\n",
            "inferior exited with status 0\n",
            "1  counted  1 location  hit 40 times  stops at hits ==2 (no later hit can stop)\n",
        ],
    );
}

#[test]
fn batch_mode_sets_and_clears_breakpoint_conditions() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted",
            "condition 1 call % 10 == 0 && last_call == call - 1",
            "run",
            "print call",
            "breakpoints",
            "condition 1",
            "continue",
            "print call",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 stops where call % 10 == 0 && last_call == call - 1 holds\n",
            "stopped at breakpoint 1 (hit 10) at ",
            "(uint64_t) call = 10\n",
            "1  counted  1 location  hit 10 times  where call % 10 == 0 && last_call == call - 1\n",
            "breakpoint 1 stops unconditionally\n",
            "stopped at breakpoint 1 (hit 11) at ",
            "(uint64_t) call = 11\n",
        ],
    );
}

#[test]
fn batch_mode_sets_amends_and_skips_watchpoint_conditions() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break caller",
            "run",
            "delete all",
            "watch last_call if call % 10 == 0",
            "continue",
            "ignore w1 15",
            "watchpoints",
            "continue",
            "condition w1",
            "hits w1 ==32",
            "continue",
            "hits w1 always",
            "condition w1 no_such_value > 1",
            "continue",
            "unwatch all",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "watchpoint 1 set on last_call: 8 bytes at 0x",
            " using 1 hardware slot, stops where call % 10 == 0\n",
            "stopped by watchpoint 1 (change, hit 10) on last_call in thread ",
            "\n  old: 9\n  new: 10\n",
            "watchpoint 1 ignores its next 15 hits\n",
            "  hit 10 times  stops at hits >=26  where call % 10 == 0\n",
            "stopped by watchpoint 1 (change, hit 30) on last_call",
            "\n  old: 29\n  new: 30\n",
            "watchpoint 1 stops unconditionally\n",
            "watchpoint 1 stops at hits ==32, hit 30 times so far\n",
            "stopped by watchpoint 1 (change, hit 32) on last_call",
            "\n  old: 31\n  new: 32\n",
            "watchpoint 1 stops at every hit, hit 32 times so far\n",
            "watchpoint 1 stops where no_such_value > 1 holds\n",
            "warning: the condition of watchpoint 1 could not be evaluated: \
             no variable is named `no_such_value` here\n",
            "stopped by watchpoint 1 (change, hit 33) on last_call",
            "deleted 1 watchpoint\n",
            "inferior exited with status 0\n",
        ],
    );
}

#[test]
fn watchpoint_condition_commands_explain_rejected_input() {
    let (_, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break caller",
            "run",
            "condition w9 x > 1",
            "hits w9 >=2",
            "ignore w9 2",
            "hits wx >=2",
            "watch last_call if",
            "watch last_call when call > 2",
            "watch last_call if call = 3",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "watchpoint 9 was not found",
            "watchpoint 9 was not found",
            "watchpoint 9 was not found",
            "usage: hits <id> <hit-condition|always>",
            "usage: watch [-w] <expression|0xaddress:byte-count> [if condition...]",
            "usage: watch [-w] <expression|0xaddress:byte-count> [if condition...]",
            "invalid condition: conditions and log messages cannot assign; compare with `==`",
        ],
    );
}

#[test]
fn hit_condition_commands_explain_rejected_input() {
    let (stdout, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "hits 1 always",
            "ignore 1 2",
            "condition 4 x > 1",
            "condition",
            "break counted 5",
            "break counted =<5",
            "break counted",
            "hits 1 ==0",
            "ignore 1 many",
            "hits one >=2",
            "condition 1 call = 3",
            // Ignoring zero hits stops at the next one.
            "hits 1 ==9",
            "ignore 1 0",
            "run",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "breakpoint 1 was not found",
            "breakpoint 1 was not found",
            "breakpoint 4 was not found",
            "usage: condition <id> [expression...]",
            "invalid hit condition: a bare count is ambiguous; write ==5 to stop only at that \
             hit or >=5 to stop at it and every later hit",
            "invalid hit condition: '=<5' is not an operator",
            "invalid hit condition: no hit can satisfy ==0",
            "usage: ignore <id> <count>",
            "usage: hits <id> <hit-condition|always>",
            "invalid condition: conditions and log messages cannot assign; compare with `==`",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 stops at its next hit\n",
            "stopped at breakpoint 1 (hit 1) at ",
        ],
    );
}

#[test]
fn strings_print_as_quoted_escaped_text() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/strings.c");
    let line = std::fs::read_to_string(&path)
        .expect("source")
        .lines()
        .position(|line| line.contains("strings stop here"))
        .expect("marker")
        + 1;
    let stdout = batch(
        &["build/test-programs/strings-c-gcc-o0"],
        &[&format!("break strings.c:{line}"), "run", "print"],
    );
    assert_in_order(
        &stdout,
        &[
            "(const char *) greeting = 0x",
            " \"hello, world\"\n",
            "(const char *) escaped = 0x",
            r#" "tab\there \"quoted\" \\ é\x80""#,
            "(const char *) long_text = 0x",
            &format!(" \"{}\"...\n", "x".repeat(256)),
            "(const char *) edge = 0x",
            " \"eeeee\"... <unreadable at 0x",
            "(char[16]) buffer = \"abc\"\n",
            "(char[4]) unterminated = \"wxyz\"\n",
            "(const char *) null_text = 0x0000000000000000\n",
            "(const char *) invalid = 0x0000000000000001 \"\"... <unreadable at 0x1>\n",
            "(unsigned char[3]) bytes = \"A\\xff\"\n",
        ],
    );
}

/// `print` shows a value as its view presents it, with its elements up to
/// the inspection's budget; `print/r` and `set views off` show it as
/// stored; and `info view` says which view presents it, or why none does.
#[test]
fn views_present_values_raw_is_one_step_away_and_info_view_explains() {
    let stdout = batch(
        &["--color", "never", "build/test-programs/containers-rust-o0"],
        &[
            "break barrier",
            "run",
            "up",
            "print ints",
            "print many",
            "print/r ints",
            "print words",
            "info view ints",
            "info view past_capacity.value.0",
            "set views off",
            "print ints",
            "info view ints",
            "set views on",
            "print ints[1] + len(ints)",
            "print",
        ],
    );
    let raw_ints = "(Vec<i32, alloc::alloc::Global>) ints = {buf = {inner = {ptr = ";
    assert_in_order(
        &stdout,
        &[
            "(Vec<i32, alloc::alloc::Global>) ints = len=3 [1, 2, 3]\n",
            "(Vec<u32, alloc::alloc::Global>) many = len=300 [0, 1, 2, 3, ",
            " 249, 250, <truncated: MemoryReads limit 256 after 256; requested 1>, <49 omitted>]\n",
            raw_ints,
            "}, len = 3}\n",
            "(Vec<alloc::string::String, alloc::alloc::Global>) words = len=2 [\"one\", \"two\"]\n",
            "`ints` has type Vec<i32, alloc::alloc::Global>\n",
            "presented by rust-std.views:",
            " `rust alloc::vec::Vec<T, _>`\nas len=3 [1, 2, 3]\nviews tried, in order:\n",
            "`past_capacity.value.0` has type Vec<i32, alloc::alloc::Global>\n",
            "binds, but shows the value as stored: check `len <= capacity` failed: \
             `len` is 9, `capacity` is 8\n",
            "values show as stored\n",
            raw_ints,
            "views are off, so it shows as stored; `set views on` turns them on\n",
            "values show as their views present them\n",
            "(integer) ints[1] + len(ints) = 5\n",
            "(PathBuf) path = \"/tmp/uscope\"\n",
            "(VecDeque<i32, alloc::alloc::Global>) ring = len=4 [1, 2, 3, 4]\n",
        ],
    );
}

/// Values are presented with the session's view files first, then the
/// project's in `.uscope/views` where the program runs and the user's, then
/// the program's own;
/// `views clear` forgets the session's, and a file with an error is
/// reported without ending the session.
/// Files given together to `views load` stack as `--views` does: later
/// files come first.
#[test]
fn view_files_loaded_together_stack_as_on_the_command_line() {
    let directory = support::ScratchDir::new("cli-view-order");
    let first = directory.path().join("first.views");
    let second = directory.path().join("second.views");
    for path in [&first, &second] {
        fs::write(path, "uscope-views 1\n").expect("write a view file");
    }
    let (first, second) = (first.display().to_string(), second.display().to_string());
    let load_both = format!("views load {first} {second}");
    let output = batch(
        &[
            "--views",
            &first,
            "--views",
            &second,
            "build/test-programs/basic",
        ],
        &["views", "views clear", &load_both, "views"],
    );
    let listings = output
        .split("values are presented with, in order:")
        .skip(1)
        .map(|listing| listing.lines().take(3).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(listings.len(), 2, "{output}");
    assert!(listings[0][1].contains("second.views"), "{output}");
    assert_eq!(listings[0], listings[1], "{output}");
}

#[test]
fn view_files_come_from_the_session_the_project_and_the_user() {
    let directory = support::ScratchDir::new("cli-view-files");
    let project = directory.path().join(".uscope/views");
    let user = directory.path().join("config/uscope/views");
    fs::create_dir_all(&project).expect("make the project's view directory");
    fs::create_dir_all(&user).expect("make the user's view directory");
    fs::write(
        project.join("points.views"),
        "uscope-views 1\nview c point {\n    format y as hex\n}\n",
    )
    .expect("write the project's views");
    fs::write(
        user.join("mine.views"),
        "uscope-views 1\nview c point {\n    show empty(\"the user's\")\n}\nview c intvec {\n    show empty(\"the user's vector\")\n}\n",
    )
    .expect("write the user's views");
    let session = directory.path().join("session.views");
    fs::write(
        &session,
        "uscope-views 1\nview c point {\n    show empty(\"the session's\")\n}\n",
    )
    .expect("write the session's views");
    let broken = directory.path().join("broken.views");
    fs::write(
        &broken,
        "uscope-views 1\nview c point {\n    show nothing\n}\n",
    )
    .expect("write a broken view file");
    let load_broken = format!("views load {}", broken.display());
    // The project is where the program runs, wherever uscope does.
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .arg("--batch")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--views")
        .arg(&session)
        .args(["-e", "break barrier", "-e", "run", "-e", "up"])
        .args(["-e", "print here", "-e", "views clear", "-e", "print here"])
        .args([
            "-e",
            "print numbers",
            "-e",
            &load_broken,
            "-e",
            "print here",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/build/test-programs/embedded-views"
        ))
        .stdin(Stdio::null())
        .output()
        .expect("run uscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = assert_success(output);
    assert_in_order(
        &stdout,
        &[
            "here = the session's",
            "forgot the loaded view files",
            "here = {x: 1, y: 0x2}",
            // The user's view comes before the program's own.
            "numbers = the user's vector",
            "loaded 1 view file",
            "here = {x: 1, y: 0x2}",
        ],
    );
    assert!(
        stderr.contains("broken.views:3:10: expected a shape"),
        "{stderr}"
    );
}

/// `uscope views check` says which view presents each of a program's types
/// that a pattern names, with no process, and fails when a view it was
/// given presents nothing or binds nothing it names; `views explain` says
/// why for one type.
#[test]
fn views_check_and_explain_a_programs_types_without_a_process() {
    let directory = support::ScratchDir::new("cli-views-check");
    let views = directory.path().join("app.views");
    fs::write(
        &views,
        "uscope-views 1\nview c point {\n    show value(z)\n}\nview c widget {\n    show empty(\"w\")\n}\n",
    )
    .expect("write the views to check");
    let program = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/build/test-programs/embedded-views"
    );
    let run = |arguments: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_uscope"))
            .current_dir(directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .arg("views")
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("run uscope views")
    };
    let clean = run(&["check".as_ref(), program.as_ref()]);
    assert_in_order(
        &assert_success(clean),
        &[
            "presented:",
            "intvec by embedded-views.views[0]:4 `c intvec`",
        ],
    );
    let failed = run(&[
        "check".as_ref(),
        program.as_ref(),
        "--views".as_ref(),
        views.as_os_str(),
    ]);
    assert!(!failed.status.success(), "{failed:?}");
    let report = String::from_utf8_lossy(&failed.stdout).into_owned();
    assert_in_order(
        &report,
        &[
            "not presented, though views name them:",
            "point",
            "app.views:2 `c point`: line 3: `z`: `z` is neither a member",
            "views that present no type:",
            "app.views:5 `c widget`",
        ],
    );
    // A view file that cannot be used fails an explanation too.
    let broken = directory.path().join("broken.views");
    fs::write(
        &broken,
        "uscope-views 1\nview c intvec {\n    show nothing\n}\n",
    )
    .expect("write a broken view file");
    let refused = run(&[
        "explain".as_ref(),
        program.as_ref(),
        "intvec".as_ref(),
        "--views".as_ref(),
        broken.as_os_str(),
    ]);
    assert!(!refused.status.success(), "{refused:?}");
    let explained = run(&["explain".as_ref(), program.as_ref(), "intvec".as_ref()]);
    assert_in_order(
        &assert_success(explained),
        &[
            "intvec in ",
            "presented by embedded-views.views[0]:4 `c intvec`",
            "views tried, in order:",
            "embedded-views.views[0]:4 `c intvec`: binds",
        ],
    );
}

/// A kernel beside a view file is loaded with it, as `NAME.wasm`; the runs
/// a presentation takes are recorded, and replay with no program; and
/// `views check` shows each kernel as the source it is built from.
#[test]
fn kernels_beside_view_files_present_values_and_their_runs_replay() {
    let directory = support::ScratchDir::new("cli-kernels");
    let root = env!("CARGO_MANIFEST_DIR");
    let views = directory.path().join("forest.views");
    fs::write(
        &views,
        "uscope-views 1\nview c tree {\n    show sequence(count) for at in kernel(\"preorder\", root, offsetof(node, child), offsetof(node, sibling))\n        => ((node *)at)->value * 10\n}\n",
    )
    .expect("write the views");
    fs::copy(
        format!("{root}/build/test-programs/tutorial-tree.wasm"),
        directory.path().join("preorder.wasm"),
    )
    .expect("put the kernel beside the views");
    let junk = directory.path().join("junk.views");
    fs::write(
        &junk,
        "uscope-views 1\nview c intvec {\n    show sequence(n) for at in kernel(\"junk\") => at\n}\n",
    )
    .expect("write views that call a broken kernel");
    fs::write(directory.path().join("junk.wasm"), b"\0asm junk").expect("write a broken kernel");
    let runs = directory.path().join("family.runs");
    let record = format!("views record {} family", runs.display());
    let load_junk = format!("views load {}", junk.display());
    let program = format!("{root}/build/test-programs/tutorial");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .arg("--batch")
        .arg("--views")
        .arg(&views)
        .args(["-e", "break barrier", "-e", "run", "-e", "up"])
        .args([
            "-e",
            "print family",
            "-e",
            &record,
            "-e",
            "info view family",
        ])
        .args(["-e", &load_junk])
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .expect("run uscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = assert_success(output);
    assert_in_order(
        &stdout,
        &[
            "family = len=5 [10, 20, 30, 40, 50]",
            "recorded 2 kernel runs to",
            "presented by ",
            "forest.views:2 `c tree`",
        ],
    );
    assert!(
        stderr.contains("junk.wasm:0:0: kernel `junk`: it is not a module a kernel may be"),
        "{stderr}"
    );
    let replay = |arguments: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_uscope"))
            .args(["views".as_ref(), "replay".as_ref(), runs.as_os_str()])
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("run uscope views replay")
    };
    let kernel = directory.path().join("preorder.wasm");
    assert_in_order(
        &assert_success(replay(&["--kernel".as_ref(), kernel.as_os_str()])),
        &[
            "run 1: kernel `preorder` reproduced",
            "run 2: kernel `preorder` reproduced",
        ],
    );
    // A run replays only with the kernel it recorded.
    let unknown = replay(&[]);
    assert!(!unknown.status.success(), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no built-in kernel is named `preorder`"),
        "{unknown:?}"
    );
    let other = replay(&[
        "--kernel".as_ref(),
        format!("{root}/views/kernels/rust-btree.wasm").as_ref(),
    ]);
    assert!(!other.status.success(), "{other:?}");
    assert!(
        String::from_utf8_lossy(&other.stdout).contains("differs"),
        "{other:?}"
    );
    let check = uscope(&["views", "check", "build/test-programs/tutorial"]);
    assert_in_order(
        &assert_success(check),
        &[
            "kernels, and what they are built from:",
            "  tree (tutorial.views[1]):",
            "    // The values of a tree whose nodes keep their children in a list, each",
        ],
    );
}

/// A program built with line tables only describes no variables or types,
/// so nothing is presented, and uscope says so rather than guessing.
#[test]
fn a_program_without_variable_information_presents_nothing() {
    let output = batch_output(
        &["build/test-programs/containers-rust-limited"],
        &["break barrier", "run", "up", "print text"],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no variable is named `text` here"),
        "{stderr}"
    );
    let check = uscope(&[
        "views",
        "check",
        "build/test-programs/containers-rust-limited",
    ]);
    assert_in_order(
        &assert_success(check),
        &["no view's pattern names any type"],
    );
}

/// `print` shows a map's entries as `key: value` and a linked structure's
/// elements, and says why a broken one shows as stored.
#[test]
fn views_print_maps_and_linked_structures() {
    let stdout = batch(
        &[
            "--color",
            "never",
            "build/test-programs/containers-cpp-gcc-o0",
        ],
        &[
            "break barrier",
            "run",
            "up",
            "print ordered",
            "print named",
            "print linked_words",
            "print forward[1] * len(forward)",
            "print looped",
            "info view overcounted",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            ") ordered = len=3 {1: 10, 2: 20, 3: 30}\n",
            ") named = len=2 {\"one\": 1, \"two\": 2}\n",
            ") linked_words = len=2 [\"a\", \"b\"]\n",
            "(integer) forward[1] * len(forward) = 10\n",
            ") looped = {",
            " <view libstdc++.views:",
            "`c++ std::list<T, _>`: cycle at element 3: it leads back to a node already visited>\n",
            "binds, but shows the value as stored: the view declares 4 elements and generates 2\n",
        ],
    );
}

/// `ptype` names a type with its path and lists its arguments, whatever
/// the producer called it.
#[test]
fn ptype_shows_qualified_names_and_template_arguments() {
    for (program, commands, expected) in [
        (
            "build/test-programs/templates-cpp-clang-o0",
            &[
                "break templates_target",
                "run",
                "ptype std::`vector<int>`",
                "ptype `Fixed<3, short>`",
            ][..],
            &[
                "type = class std::vector<int, std::allocator<int> > {",
                "arguments: int, std::allocator<int>\n",
                "type = struct Fixed<3, short> {\n    short[3] items;\n}\narguments: 3, short\n",
            ][..],
        ),
        (
            "build/test-programs/generics-rust-o0",
            &[
                "break generics_target",
                "run",
                "ptype alloc::vec::`Vec<i32>`",
            ][..],
            &[
                "type = struct alloc::vec::Vec<i32, alloc::alloc::Global> {",
                "arguments: i32, alloc::alloc::Global\n",
            ][..],
        ),
    ] {
        let stdout = batch(&[program], commands);
        assert_in_order(&stdout, expected);
    }
}

#[test]
fn set_changes_values_the_program_then_uses() {
    let stdout = batch(
        &["build/test-programs/variables-gcc-o0"],
        &[
            "break pointer_target",
            "run",
            "up",
            "set signed_int = signed_int + 1",
            "set var boolean = false",
            "delete all",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "(int) signed_int = -1234566\n",
            "(_Bool) boolean = false\n",
            "inferior exited with status 1\n",
        ],
    );
}

#[test]
fn print_and_p_render_scalars_and_print_lists_every_variable() {
    let stdout = batch(
        &["build/test-programs/variables-gcc-o0"],
        &["break variables.c:108", "run", "p signed_int", "print"],
    );
    assert_eq!(
        stdout.matches("(int) signed_int = -1234567").count(),
        2,
        "{stdout}"
    );
    for expected in [
        "(char) character = 65 'A'",
        "(float) single = 1.25",
        "(double) double_precision = -2.5",
        "(long double) extended = 3.125",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }
}

#[test]
fn info_symbol_and_x_describe_memory_by_address() {
    let executable = fixture("build/test-programs/variables-gcc-nopie");
    let main = symbol_address(&executable, "main");
    let data = symbol_address(&executable, "pointer_parameter_value");
    // _dl_relocate_static_pie is five bytes long and followed by padding.
    let padding = symbol_address(&executable, "_dl_relocate_static_pie") + 8;
    let commands = [
        "break main".to_owned(),
        "run".to_owned(),
        format!("info symbol {:#x}", main + 4),
        format!("info symbol {:#x}", data + 2),
        format!("info symbol {padding:#x}"),
        "info symbol 0x8".to_owned(),
        format!("x {data:#x} 4"),
    ];
    let commands = commands.iter().map(String::as_str).collect::<Vec<_>>();
    let stdout = batch(&["build/test-programs/variables-gcc-nopie"], &commands);
    let path = executable.canonicalize().expect("canonical fixture path");
    let path = path.display();
    for expected in [
        format!("main+0x4 in section .text of {path}"),
        format!("pointer_parameter_value+0x2 in section .data of {path}"),
        format!("no symbol contains {padding:#x} in section .text of {path}"),
        "no loaded module contains 0x8".to_owned(),
    ] {
        assert!(
            stdout.lines().any(|line| line == expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    assert!(
        stdout.contains(&format!("{data:#018x}: 2a 00 00 00")),
        "{stdout}"
    );
    assert!(stdout.contains("|*...            |"), "{stdout}");
}

#[test]
fn print_dereferences_pointer_chains_and_points_at_what_cannot_be() {
    let (stdout, stderr) = piped(
        &["build/test-programs/variables-gcc-o0"],
        &[
            "break variables.c:68",
            "run",
            "p pointer",
            "p *pointer",
            "p **pointer_pointer",
            "p *null_pointer",
            "p *invalid_pointer",
            "p *void_pointer",
            "p *pointee",
            "p **pointer",
        ],
    );
    assert!(stdout.contains("(int *) pointer = 0x"), "{stdout}");
    assert!(stdout.contains("(int) *pointer = 42"), "{stdout}");
    assert!(stdout.contains("(int) **pointer_pointer = 42"), "{stdout}");
    // A failed dereference is labeled with the pointee type (int), not the
    // operand's pointer type (int *).
    assert!(
        stdout.contains("(int) *null_pointer = <unavailable: cannot dereference a null pointer>"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("(int *) *null_pointer"),
        "failed dereference must not render the pointer's own type: {stdout}"
    );
    assert!(
        stdout.contains("*invalid_pointer = <unavailable:"),
        "{stdout}"
    );
    // Dereferencing what cannot be dereferenced is the expression's error,
    // pointed at.
    assert_in_order(
        &stderr,
        &[
            "`void_pointer` points at void",
            "\n     ^^^^^^^^^^^^\n",
            "`pointee` is not a pointer",
            "\n     ^^^^^^^\n",
            "`*pointer` is not a pointer",
            "\n     ^^^^^^^^\n",
        ],
    );
}

#[test]
fn print_resolves_recursive_types_from_dwarf_four_and_five_type_units() {
    for program in [
        "build/test-programs/types-cpp-gcc-dwarf4",
        "build/test-programs/types-cpp-gcc-dwarf5",
    ] {
        let stdout = batch(
            &[program],
            &[
                "break types.cpp:31",
                "run",
                "p recursive.value",
                "p mutual.peer",
            ],
        );
        assert!(
            stdout.contains("(volatile alias_chain) recursive.value = 9"),
            "{program}: {stdout}"
        );
        assert!(
            stdout.contains("(right *) mutual.peer = 0x"),
            "{program}: {stdout}"
        );
    }
}

#[test]
fn print_renders_aggregates_slices_and_ranges() {
    let stdout = batch(
        &["build/test-programs/records-c-gcc-o0"],
        &[
            "break inspect_records",
            "run",
            "print global_record",
            "print *bits",
            "print *records",
            "print huge_array[1048576]",
            "print huge_array[3..7]",
            "print (*records)[1].values[1]",
        ],
    );
    for expected in [
        "global_record = {inner = {signed_value = -7, unsigned_value = 9}, values = [20, 22]}",
        "*bits = {negative = -3, first = 5, second = 42}",
        "values = [43, 44]",
        "(unsigned char) huge_array[1048576] = 0",
        "huge_array[3..7] = [0, 0, 0, 0]",
        "(int32_t) (*records)[1].values[1] = 44",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }

    let stdout = batch(
        &["build/test-programs/variables-rust-o0"],
        &["break variables.rs:67", "run", "print slice"],
    );
    assert!(stdout.contains("(&[i32]) slice = [20, 22]"), "{stdout}");
}

#[test]
fn print_renders_symbolic_enums_variants_and_raw_unions() {
    let stdout = batch(
        &["build/test-programs/enums-rust-o0"],
        &[
            "break inspect_enum",
            "run",
            "print *value",
            "print *fieldless",
            "print *wide",
            "frame 1",
            "print",
        ],
    );
    // A sum type shows as its active variant.
    for expected in [
        "*value = Integer(42)",
        "*fieldless = Negative (-3)",
        "*wide = Huge (1267650600228229401496703205385)",
        "value = Integer(42)",
        "optional = Some(0x",
        "empty = None",
        "done = Ok(())",
        "failed = Err(5)",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }

    let stdout = batch(
        &["build/test-programs/enums-c-gcc-o0"],
        &["break inspect_enums", "run", "print *raw"],
    );
    assert!(
        stdout.contains("*raw = {integer = 42, floating = ")
            && stdout.contains("} <active member unknown>"),
        "{stdout}"
    );
}

#[test]
fn globals_lists_metadata_and_print_accepts_exact_qualification() {
    let stdout = batch(
        &["build/test-programs/globals-c-gcc-o0"],
        &[
            "globals duplicate",
            "break main.c:9",
            "run",
            "print one.c::duplicate",
            "continue",
        ],
    );
    assert!(stdout.contains("duplicate ("), "{stdout}");
    assert!(stdout.contains("duplicate = 201"), "{stdout}");
}

#[test]
fn batch_mode_lists_threads_and_steps_one_instruction() {
    let stdout = batch(
        &[BASIC],
        &["break breakpoint_target", "run", "threads", "stepi"],
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("* ") && line.contains(" stopped")),
        "{stdout}"
    );
    assert!(stdout.contains("stopped after instruction step"));
}

#[test]
fn stops_and_list_show_source_from_any_working_directory() {
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .current_dir("/")
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
            "--eval",
            "l",
        ])
        .arg(fixture(BASIC))
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);
    assert_eq!(
        stdout.matches("tests/fixtures/c/basic.c:6").count(),
        2,
        "{stdout}"
    );
    assert_eq!(
        stdout.matches("=> 6 |     return uscope_value;").count(),
        2,
        "{stdout}"
    );
    assert!(stdout.contains("   8 |"), "{stdout}");
}

#[test]
fn source_maps_read_sources_recorded_under_another_directory() {
    let commands = ["break breakpoint_target", "run"];
    let unmapped = batch(&["build/test-programs/basic-relocated"], &commands);
    assert!(
        unmapped.contains(
            "source unavailable: source file /nonexistent/uscope/tests/fixtures/c/basic.c does not exist"
        ),
        "{unmapped}"
    );

    let repository = env!("CARGO_MANIFEST_DIR");
    let stdout = batch(
        &[
            "--source-map",
            "/nonexistent/uscope",
            repository,
            "build/test-programs/basic-relocated",
        ],
        &commands,
    );
    assert!(
        stdout.contains(&format!("{repository}/tests/fixtures/c/basic.c:6")),
        "{stdout}"
    );
    assert!(
        stdout.contains("=> 6 |     return uscope_value;"),
        "{stdout}"
    );
}

#[test]
fn ctrl_c_pauses_a_running_inferior_before_accepting_more_commands() {
    let mut uscope = Uscope::spawn(
        Command::new(env!("CARGO_BIN_EXE_uscope")).arg(fixture("build/test-programs/spin")),
    );
    uscope.send("break main\nrun\nthreads\n");
    uscope.line("the breakpoint stop", |line| {
        line.starts_with("stopped at breakpoint 1")
    });
    let thread = uscope.line("the stopped thread", |line| line.starts_with("* "));
    let inferior: u32 = thread
        .split_whitespace()
        .nth(1)
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("no thread ID in {thread:?}"));

    // main's loop is a jump to itself, so the breakpoint must go before the
    // inferior can run it. Only the continue can make the stopped inferior
    // runnable again, so once it is, Ctrl-C interrupts the loop.
    uscope.send("delete 1\ncontinue\n");
    support::wait_until("the inferior runs main's loop", || {
        process_state(inferior) == Some('R')
    });
    kill(
        Pid::from_raw(i32::try_from(uscope.id()).expect("debugger PID fits i32")),
        Signal::SIGINT,
    )
    .expect("pause uscope");
    uscope.line("the pause", |line| line == "inferior paused");

    // End of input shuts the session down, killing and reaping the inferior.
    uscope.send("where\n");
    uscope.close_stdin();
    let stdout = assert_success(uscope.finish());
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("main at ") && line.contains("spin.c:10 ")),
        "the pause did not land in main's loop:\n{stdout}"
    );
    assert_eq!(
        process_state(inferior),
        None,
        "inferior {inferior} survived debugger shutdown"
    );
}

#[test]
fn continuing_past_a_sigint_stop_discards_the_interrupt() {
    // A terminal Ctrl-C signals the inferior as well as uscope. Like gdb, the
    // CLI must not deliver that SIGINT, which would terminate the inferior.
    let stdout = batch(&["build/test-programs/interrupt"], &["run", "continue"]);
    assert!(stdout.contains("stopped by SIGINT"), "{stdout}");
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

#[test]
fn core_dumps_are_inspected_in_batch_mode_without_executing() {
    let output = batch_output(
        &["--core", SEGV_CORE],
        &[
            "where",
            "bt",
            "threads",
            "print depth",
            "print crash_library_tls",
            "info core",
        ],
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = assert_success(output);
    assert!(stdout.starts_with("crash_segv at "), "{stdout}");
    assert!(stdout.contains("#1 "), "{stdout}");
    assert!(stdout.contains("in main at "), "{stdout}");
    assert!(
        stdout.contains("unwind stopped: the outermost frame has no caller"),
        "{stdout}"
    );
    assert_eq!(
        stdout
            .matches("process terminated by SIGSEGV (SEGV_MAPERR) at 0x0 (0xb)")
            .count(),
        1,
        "only the faulting thread carries the reason: {stdout}"
    );
    assert!(stdout.contains("(int32_t) depth = 3"), "{stdout}");
    assert!(stdout.contains("crash_library_tls = 654"), "{stdout}");
    assert!(
        stdout.contains("signal: SIGSEGV (SEGV_MAPERR) at 0x0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("crash-gcc-o0 module 0 verified by build-id"),
        "{stdout}"
    );
    assert_no_sgr(&stdout);

    let commands = [
        "run",
        "continue",
        "step",
        "next",
        "finish",
        "stepi",
        "break main",
    ];
    let (_, stderr) = piped(&["--core", SEGV_CORE], &commands);
    assert_eq!(
        stderr
            .matches("a post-mortem core dump cannot execute, be modified, or hold breakpoints")
            .count(),
        commands.len(),
        "{stderr}"
    );
}

#[test]
fn core_banner_reports_the_signal_and_every_unproven_module() {
    let stdout = assert_success(uscope(&["--core", SEGV_CORE]));
    let mut lines = stdout.lines();
    let banner = lines.next().unwrap_or_default();
    assert!(
        banner.starts_with("opened core dump ") && banner.contains("of crash-gcc-o0 (process "),
        "{stdout}"
    );
    assert_eq!(
        lines.next(),
        Some("process terminated by SIGSEGV (SEGV_MAPERR) at 0x0 (0xb)")
    );
    assert!(!stdout.contains("warning"), "{stdout}");

    // Warnings go to stderr, keeping stdout clean.
    let missing = uscope(&[
        "--core",
        "build/test-programs/core-missing-library/crash.core",
    ]);
    let stderr = String::from_utf8_lossy(&missing.stderr).into_owned();
    assert!(
        stderr.contains("core-missing-library/libcrash.so is missing; its frames and unsaved memory are unavailable"),
        "{stderr}"
    );
    assert!(!assert_success(missing).contains("warning"));
    let batch = uscope(&[
        "--core",
        "build/test-programs/core-missing-library/crash.core",
        "--batch",
    ]);
    assert!(String::from_utf8_lossy(&batch.stderr).contains("libcrash.so is missing"));
    assert_eq!(assert_success(batch), "");
}

#[test]
fn allowed_module_mismatches_are_named_and_reported() {
    let allowed = batch_output(
        &[
            "--core",
            SEGV_CORE,
            "build/test-programs/crash-gcc-o0-rebuilt",
            "--allow-module-mismatch",
        ],
        &["info core"],
    );
    let stderr = String::from_utf8_lossy(&allowed.stderr).into_owned();
    assert!(
        stderr.contains("crash-gcc-o0-rebuilt does not match the dump (the build-id note differs); using its metadata anyway"),
        "{stderr}"
    );
    // The line names the file used when it is not at the recorded path.
    let stdout = assert_success(allowed);
    let rebuilt = fs::canonicalize(fixture("build/test-programs/crash-gcc-o0-rebuilt"))
        .expect("canonical fixture");
    assert!(
        stdout.contains(&format!(
            "crash-gcc-o0 module 0 from {} mismatched: the build-id note differs",
            rebuilt.display()
        )),
        "{stdout}"
    );
}

#[test]
fn core_module_searches_name_the_files_they_use_and_those_still_missing() {
    // Without the C library, its recorded build-id says what to supply.
    let directory = support::ScratchDir::new("cli-module-path");
    for name in ["crash-gcc-o0", "libcrash.so"] {
        fs::copy(
            fixture(&format!("build/test-programs/{name}")),
            directory.path().join(name),
        )
        .expect("copy module");
    }
    let module_path = directory.path().to_str().expect("UTF-8 scratch path");
    let partial = batch_output(
        &[
            "--core",
            "build/test-programs/core-foreign/crash.core",
            "--module-path",
            module_path,
        ],
        &["info core"],
    );
    let stderr = String::from_utf8_lossy(&partial.stderr).into_owned();
    let stdout = assert_success(partial);
    let libc = object::File::parse(
        fs::read(fixture("build/test-programs/libc-foreign.so.6"))
            .expect("read foreign C library")
            .as_slice(),
    )
    .expect("parse foreign C library")
    .build_id()
    .expect("read build-id")
    .expect("build-id note")
    .iter()
    .fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    });
    assert!(
        stderr.contains(&format!(
            "core-foreign/libc.so.6 is missing; its frames and unsaved memory are unavailable (build-id {libc})"
        )),
        "{stderr}"
    );
    assert!(
        stdout.contains(&format!("core-foreign/libc.so.6 missing (build-id {libc})")),
        "{stdout}"
    );
    let canonical = directory
        .path()
        .canonicalize()
        .expect("canonical scratch path");
    for name in ["crash-gcc-o0", "libcrash.so"] {
        let line = stdout
            .lines()
            .find(|line| line.contains(&format!("core-foreign/{name} module")))
            .unwrap_or_else(|| panic!("no {name} line:\n{stdout}"));
        assert!(
            line.ends_with(&format!(
                " from {} verified by build-id",
                canonical.join(name).display()
            )),
            "{line}"
        );
    }
}

#[test]
fn backtraces_and_where_name_code_without_debug_info_by_symbol_and_module() {
    let stdout = batch(
        &["--core", "build/test-programs/elf-symbols-stripped.core"],
        &["bt", "where"],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let frame = |needle: &str| {
        lines
            .iter()
            .find(|line| line.starts_with('#') && line.contains(needle))
            .unwrap_or_else(|| panic!("no frame containing {needle:?}:\n{stdout}"))
    };
    // An unsized symbol says so; code no symbol names stays unknown, even
    // where stripping removed a static function between named ones.
    assert!(
        frame(" in asm_unsized+0x6 ")
            .ends_with(" (unsized symbol) from libelf-symbols-stripped.so"),
        "{stdout}"
    );
    assert!(frame(" in asm_sized+0x6 ").ends_with(" from libelf-symbols-stripped.so"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with(" in <unknown> from libelf-symbols-stripped.so"))
            .count(),
        2,
        "{stdout}"
    );
    assert!(!stdout.contains("lib_static_helper"), "{stdout}");
    // Frames with source keep showing it instead of a module.
    assert!(frame(" in chain_gap at ").ends_with("tests/fixtures/c/elf-symbols/main.c:69"));
    assert!(frame(" in _start+0x").ends_with(" from elf-symbols-stripped"));
    let location = lines.last().expect("where output");
    assert!(
        location.starts_with("lib_fault at 0x")
            && location.ends_with(" from libelf-symbols-stripped.so"),
        "{stdout}"
    );
}

#[test]
fn frame_commands_move_through_caller_frames_and_show_their_source() {
    let stdout = batch(
        &["--core", FRAMES_CORE],
        &[
            "frame 5",
            "print depth",
            "up",
            "print depth",
            "where",
            "down 2",
            "list",
            "frame",
            "up 100",
            "down",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let recursive_call = format!(
        "frames.c:{}",
        frames_line("int64_t below = frames_recurse(depth - 1, seed);")
    );
    let innermost_call = frames_line("return frames_keep(seed) + level;");

    let frame = line_index(&lines, 0, |line| {
        line.starts_with("#5 ")
            && line.contains(" in frames_recurse at ")
            && line.ends_with(&recursive_call)
    });
    // The frame's source follows it, marking its call.
    let marked = line_index(&lines, frame, |line| line.starts_with("=> "));
    assert!(lines[marked].contains("frames_recurse(depth - 1, seed)"));
    let depth = line_index(&lines, marked, |line| line == "(int64_t) depth = 1");
    let up = line_index(&lines, depth, |line| line.starts_with("#6 "));
    let depth = line_index(&lines, up, |line| line == "(int64_t) depth = 2");
    let location = line_index(&lines, depth, |line| {
        line.starts_with("frames_recurse at ") && line.contains(&format!("{recursive_call} (0x"))
    });
    let down = line_index(&lines, location, |line| {
        line.starts_with("#4 ") && line.ends_with(&format!("frames.c:{innermost_call}"))
    });
    let listed = line_index(&lines, down + 1, |line| {
        line.starts_with(&format!("=> {innermost_call} |"))
    });
    let shown = line_index(&lines, listed, |line| line.starts_with("#4 "));
    let outermost = line_index(&lines, shown, |line| {
        line.starts_with("#11 ") && line.contains(" in _start+0x")
    });
    line_index(&lines, outermost, |line| line.starts_with("#10 "));
}

#[test]
fn frame_commands_report_the_ends_of_the_stack() {
    let (_, stderr) = piped(
        &["--core", FRAMES_CORE],
        &["down", "frame 99", "up x", "up 100", "up"],
    );
    assert_in_order(
        &stderr,
        &[
            "the innermost frame is selected",
            "frame 99 does not exist; the backtrace has 12 frames",
            "usage: up [count]",
            "the outermost frame is selected",
        ],
    );
}

#[test]
fn disassembly_and_finish_follow_the_selected_frame() {
    // An outer frame's function is shown around its call, marking where
    // execution returns.
    let stdout = batch(&["--core", FRAMES_CORE], &["frame 3", "disassemble"]);
    let lines = stdout.lines().collect::<Vec<_>>();
    let function = line_index(&lines, 0, |line| {
        line == "function frames_keep in frames-gcc-o0:"
    });
    let marked = line_index(&lines, function, |line| line.starts_with("=> "));
    assert!(
        lines[marked - 1].contains("call") && lines[marked - 1].ends_with("<frames_relay>"),
        "{stdout}"
    );

    let stdout = batch(
        &["build/test-programs/frames-gcc-o0"],
        &[
            &format!(
                "break frames.c:{}",
                frames_line("frames_sink = leaf_local;")
            ),
            "run",
            "frame 5",
            "finish",
            "print depth",
            "backtrace",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let finished = line_index(&lines, 0, |line| line == "(int64_t) depth = 2");
    line_index(&lines, finished, |line| {
        line.starts_with("#0 ") && line.contains(" in frames_recurse at ")
    });
    // Depths 2 and 3 remain below main.
    line_index(&lines, finished, |line| {
        line.starts_with("#2 ") && line.contains(" in main at ")
    });
}

#[test]
fn a_line_breakpoint_inside_an_inline_body_stops_in_the_inline_frame() {
    let stdout = batch(
        &["build/test-programs/variables-inline-gcc-o0"],
        &[
            "break variables-inline.c:6",
            "run",
            "backtrace",
            "up",
            "disassemble",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "#0  ",
            " in inline_target at ",
            "variables-inline.c:6",
            "#1  ",
            " in inline_caller at ",
            "variables-inline.c:12",
            "function inline_caller in variables-inline-gcc-o0:",
        ],
    );
}

#[test]
fn backtraces_demangle_rust_symbols() {
    let stdout = batch(
        &["--core", "build/test-programs/crash-rust-nodebug.core"],
        &["bt"],
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains(" in crash::crash_now+0x")
                && line.ends_with(" from crash-rust-nodebug")),
        "{stdout}"
    );
}

#[test]
fn where_reports_instructions_outside_every_module_by_address() {
    let stdout = batch(
        &["--core", "build/test-programs/null-call.core"],
        &["where"],
    );
    assert_eq!(
        stdout.trim_end(),
        "<unknown> at 0x0000000000000000 outside every loaded module"
    );
}

#[test]
fn watch_script_reports_values_lists_and_deletes_watchpoints() {
    let stdout = assert_success(uscope(&[
        "--batch",
        "-c",
        "tests/fixtures/watch.uscope",
        "build/test-programs/watch-gcc-o0",
    ]));
    assert_no_sgr(&stdout);

    let set = stdout
        .lines()
        .find(|line| line.starts_with("watchpoint 1 set on watch_i32: 4 bytes at 0x"))
        .unwrap_or_else(|| panic!("missing set confirmation:\n{stdout}"));
    assert!(set.ends_with("using 1 hardware slot"), "{set}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("1  change  watch_i32  4 bytes at 0x")),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("2  write  watch_u64  8 bytes at 0x")),
        "{stdout}"
    );
    for (old, new) in [(0, 1), (1, 2), (2, 42)] {
        assert!(
            stdout.contains(&format!("\n  old: {old}\n  new: {new}\n")),
            "missing {old} -> {new}:\n{stdout}"
        );
    }
    // `watch` skips the store of 2 over 2; `watch -w` reports the failed
    // compare-exchange's store of the value already there.
    assert_eq!(
        stdout
            .matches("stopped by watchpoint 1 (change, hit ")
            .count(),
        3
    );
    assert_eq!(
        stdout
            .matches("stopped by watchpoint 2 (write, hit ")
            .count(),
        2
    );
    assert!(
        stdout.contains("\n  value: 4919131752989213764 (unchanged)\n"),
        "{stdout}"
    );
    assert!(stdout.contains("deleted watchpoint 1"));
    assert!(stdout.contains("deleted 1 watchpoint\n"), "{stdout}");
    assert!(
        stdout.contains("watch.c:"),
        "watch stops show source:\n{stdout}"
    );
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

#[test]
fn access_and_location_watchpoints_render_their_kind_and_slots() {
    let stdout = batch(
        &["build/test-programs/watch-gcc-o0"],
        &[
            "break read_access",
            "run",
            "awatch watch_i32",
            "watch watch_packed.field",
            "info watchpoints",
            "continue",
            "unwatch all",
        ],
    );
    assert!(stdout.contains("1  read/write  watch_i32"), "{stdout}");
    assert!(
        stdout.contains("watchpoint 2 set on watch_packed.field: 4 bytes at 0x")
            && stdout.contains("using 3 hardware slots"),
        "{stdout}"
    );
    assert!(
        stdout.contains("stopped by watchpoint 1 (read/write, hit 1) on watch_i32"),
        "{stdout}"
    );
    assert!(stdout.contains("\n  value: 42 (unchanged)\n"), "{stdout}");
    assert!(stdout.contains("deleted 2 watchpoints"), "{stdout}");
}

#[test]
fn signed_watched_values_render_with_their_sign() {
    let stdout = batch(
        &["build/test-programs/watch-gcc-o0"],
        &[
            "break read_access",
            "run",
            "watch watch_sink",
            "continue",
            "continue",
        ],
    );
    assert!(stdout.contains("\n  new: 42\n"), "{stdout}");
    assert!(stdout.contains("\n  old: 42\n  new: -42\n"), "{stdout}");
}

#[test]
fn watch_commands_explain_rejected_input() {
    let usage = "usage: watch [-w] <expression|0xaddress:byte-count>";
    let (_, stderr) = piped(
        &["build/test-programs/watch-gcc-o0"],
        &[
            "watch",
            "watch -w",
            "break scalar_stores",
            "run",
            "rwatch watch_i32",
            "watch 0x1000:many",
            "watch -x watch_i32",
            "watch watch_array[0..2]",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            usage,
            usage,
            "read watchpoints are unsupported by this target's debug hardware",
            usage,
            usage,
            "a range cannot be watched; watch one element",
        ],
    );
}

#[test]
fn disassemble_renders_the_stopped_function_with_named_targets_and_source_lines() {
    let stdout = batch(
        &["build/test-programs/disassembly-gcc-o0"],
        &["break main", "run", "disassemble"],
    );
    let listing = stdout
        .split_once("function main in disassembly-gcc-o0:\n")
        .unwrap_or_else(|| panic!("no function header in:\n{stdout}"))
        .1;
    let lines = listing.lines().collect::<Vec<_>>();
    let stopped = lines
        .iter()
        .position(|line| line.starts_with("=> "))
        .unwrap_or_else(|| panic!("no stopped instruction in:\n{listing}"));
    // The stop follows the frame setup, under its source line.
    assert!(lines[stopped].contains(" <main+0x4>: "), "{listing}");
    assert!(
        lines[stopped].ends_with(" <disasm_marked_data>") && lines[stopped].contains("call 0x"),
        "{listing}"
    );
    assert!(
        lines[stopped - 1].ends_with("tests/fixtures/c/disassembly/main.c:17"),
        "{listing}"
    );
    for expected in [
        "call 0x",
        " <disasm_helper>",
        "# 0x",
        " <disasm_counter>",
        " <.plt+0x",
        "push rbp",
    ] {
        assert!(
            listing.contains(expected),
            "missing {expected:?} in:\n{listing}"
        );
    }
    assert_no_sgr(&stdout);
}

#[test]
fn disassemble_reports_data_inside_code_and_unproven_addresses() {
    let executable = fixture("build/test-programs/disassembly-clang-o2-nopie");
    let hidden = symbol_address(&executable, "disasm_hidden_data");
    let inside = format!("disassemble {:#x} 2", hidden + 4);
    let stdout = batch(
        &[
            "--disassembly-syntax",
            "att",
            "build/test-programs/disassembly-clang-o2-nopie",
        ],
        &[
            "break main",
            "run",
            "disassemble disasm_marked_data",
            "disassemble disasm_hidden_data",
            &inside,
        ],
    );
    for expected in [
        format!(
            "this instruction overlaps {:#x}, where ",
            symbol_address(&executable, "disasm_marked_resume")
        ),
        "proves an instruction begins; decoding resumes there".to_owned(),
        format!(
            "this instruction extends past the end of the range at {:#x}",
            hidden + 10
        ),
        format!(
            "{:#x} lies inside the instruction at {:#x} when decoding from {hidden:#x}; it is probably not an instruction start",
            hidden + 4,
            hidden + 2
        ),
        "mov $1, %eax".to_owned(),
        "movabs".to_owned(),
    ] {
        assert!(
            stdout.contains(&expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
}

#[test]
fn disassemble_names_the_targets_indirect_branches_read_at_the_stop() {
    let executable = fixture("build/test-programs/disassembly-clang-o2-nopie");
    let address = |name| symbol_address(&executable, name);
    let data = fs::read(&executable).expect("read fixture");
    let object = object::File::parse(data.as_slice()).expect("parse fixture");
    let section = |name| {
        object
            .section_by_name(name)
            .unwrap_or_else(|| panic!("no section {name}"))
            .address()
    };
    let (plt, got_plt) = (section(".plt"), section(".got.plt"));
    let commands = [
        format!("break {:#x}", address("disasm_call_method")),
        format!("break {:#x}", address("disasm_call_register")),
        format!("break {:#x}", address("disasm_return")),
        "run\ndisassemble".to_owned(),
        format!("disassemble {:#x} 1", plt + 0x10),
        format!(
            "continue\ndisassemble {:#x} 1",
            address("disasm_call_register")
        ),
        format!("continue\ndisassemble {:#x} 1", address("disasm_return")),
        "disassemble disasm_indirect_forms".to_owned(),
    ];
    let commands = commands
        .iter()
        .flat_map(|command| command.lines())
        .collect::<Vec<_>>();
    let stdout = batch(
        &["build/test-programs/disassembly-clang-o2-nopie"],
        &commands,
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let find = |needle: &str, stopped: bool| {
        lines
            .iter()
            .find(|line| line.contains(needle) && line.starts_with("=> ") == stopped)
            .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{stdout}"))
            .to_owned()
    };
    // At the stop, registers hold the target or address its slot.
    assert!(
        find("call qword ptr [rax+8]", true).ends_with(&format!(
            "  # {:#x} <disasm_methods+0x8> -> {:#x} <disasm_target_two>",
            address("disasm_methods") + 8,
            address("disasm_target_two")
        )),
        "{stdout}"
    );
    assert!(
        find("call rcx", true).ends_with(&format!(
            "call rcx  # -> {:#x} <disasm_target_four>",
            address("disasm_target_four")
        )),
        "{stdout}"
    );
    let ret = find("ret  # ", true);
    assert!(ret.contains(" <main+0x"), "{ret}");
    // Away from the stop, only slots the instruction addresses are read.
    assert!(find("call rcx", false).ends_with("call rcx"), "{stdout}");
    assert!(
        find(
            &format!("# {:#x} <disasm_slot>", address("disasm_slot")),
            false
        )
        .ends_with(&format!(
            "-> {:#x} <disasm_target_one>",
            address("disasm_target_one")
        )),
        "{stdout}"
    );
    let got = find(" <.got+0x", false);
    assert!(
        got.contains(" -> 0x7") && got.ends_with(" in libc.so.6>"),
        "{got}"
    );
    // Lazy binding has not yet filled the stub's slot, which is named once.
    let stub = find("jmp qword ptr [rip+", false);
    assert!(
        stub.ends_with(&format!(
            "jmp qword ptr [rip+{:#x}]  # {:#x} <.got.plt+0x18> -> {:#x} <.plt+0x16>",
            got_plt + 0x18 - (plt + 0x16),
            got_plt + 0x18,
            plt + 0x16
        )),
        "{stub}"
    );
    assert!(
        find("jmp qword ptr [0]", false).ends_with("  # 0x0 -> memory inaccessible at 0x0"),
        "{stdout}"
    );
    // Far transfers, interrupt returns, and branches whose operand size
    // differs between Intel and AMD processors say they are not computed.
    for form in ["jmp far fword ptr [rax]", "jmp rax", "iretq"] {
        assert!(
            find(form, false).ends_with(&format!(
                "{form}  # -> not computed for this form of branch"
            )),
            "{stdout}"
        );
    }
    assert_no_sgr(&stdout);
}

#[test]
fn disassemble_says_when_a_restarted_system_call_hides_a_stopped_target() {
    let target = support::ExternalProcess::spawn(&fixture("build/test-programs/attach-restart"));
    // Attaching must interrupt pause(2).
    support::wait_for_system_call(target.process_id(), 34);
    let process = target.process_id().get().to_string();
    let output = batch_output(&["--attach", &process], &["disassemble"]);
    drop(target);
    let stdout = assert_success(output);
    let stopped = stdout
        .lines()
        .find(|line| line.starts_with("=> "))
        .unwrap_or_else(|| panic!("no stopped instruction in:\n{stdout}"));
    assert!(
        stopped.ends_with(
            "jmp rax  # -> unknown: a restarted system call replaces a register it uses"
        ),
        "{stdout}"
    );
}

#[test]
fn disassemble_shows_a_stop_outside_every_function_and_rejects_bad_requests() {
    // A call through a null pointer stops where no module or function is.
    let stdout = batch(&["build/test-programs/null-call"], &["run", "disassemble"]);
    assert!(
        stdout.contains("memory inaccessible at 0x0"),
        "missing unreadable stop in:\n{stdout}"
    );

    let count = "instruction count must be between 1 and 4096";
    let (_, stderr) = piped(
        &["build/test-programs/disassembly-gcc-o0"],
        &[
            "break main",
            "run",
            "disassemble 0x8",
            "disassemble main 0",
            "disassemble main 4097",
            "disassemble missing_function",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "no function or code symbol contains 0x8; give an instruction count to disassemble from it",
            count,
            count,
            "no symbol named 'missing_function'",
        ],
    );
}

#[test]
fn disassemble_reads_code_from_core_dumps() {
    let stdout = batch(
        &["--core", "build/test-programs/crash-gcc-o2-nopie-segv.core"],
        &["disassemble", "disassemble main"],
    );
    assert!(
        stdout.contains("function crash_segv in crash-gcc-o2-nopie:"),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("=> 0x")),
        "{stdout}"
    );
    // The optimized main has a cold range of its own.
    assert!(
        stdout.contains("range 1 of 2: 0x4010a0..0x4010a5"),
        "{stdout}"
    );
    assert!(stdout.contains(" <main.cold>: "), "{stdout}");
}

#[test]
fn run_starts_the_program_with_the_command_line_arguments_directory_and_environment() {
    let directory = support::ScratchDir::new("cli-launch-directory");
    let directory_argument = directory.path().to_str().expect("UTF-8 scratch path");
    let output = batch_output(
        &[
            "--cwd",
            directory_argument,
            "--env",
            "USCOPE_FIXTURE_VALUE=a=b",
            "build/test-programs/process-environment",
            "--",
            "--not-an-option",
            "two words",
        ],
        &["run"],
    );
    let stdout = assert_success(output);
    let directory = directory
        .path()
        .canonicalize()
        .expect("canonical scratch directory");
    assert!(
        stdout.contains(&format!(
            "argument 1: --not-an-option\nargument 2: two words\nvalue: a=b\nremoved: absent\ndirectory: {}\n",
            directory.display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("exited with status 3"), "{stdout}");
}

#[test]
fn registers_show_the_selected_frames_own_values() {
    let stdout = batch(
        &["--core", FRAMES_CORE],
        &["registers", "frame 5", "registers"],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let innermost = line_index(&lines, 0, |line| line.starts_with("rdi "));
    assert!(!lines[innermost].contains("<not saved>"), "{stdout}");
    let selected = line_index(&lines, innermost, |line| line.starts_with("#5 "));
    // A callee may overwrite rdi without saving it, so frame 5's value is
    // unknown, while its instruction pointer is its own return address.
    let rdi = line_index(&lines, selected, |line| line.starts_with("rdi "));
    assert!(lines[rdi].ends_with("<not saved>"), "{stdout}");
    let rip = line_index(&lines, selected, |line| line.starts_with("rip "));
    let frame_address = lines[selected]
        .split_whitespace()
        .find(|word| word.starts_with("0x"))
        .expect("frame address");
    assert!(lines[rip].ends_with(frame_address), "{stdout}");
}

#[test]
fn handle_shows_and_changes_signal_policies_like_gdb() {
    let stdout = batch(
        &["build/test-programs/signal-policy"],
        &[
            "handle SIGUSR1",
            "handle usr1 nostop",
            "handle SIGALRM print nopass",
            "handle SIG35 noprint",
            "run",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let shown = |name: &str, stop: &str, print: &str, pass: &str| {
        format!("{name:<8}  {stop:<4}  {print:<5}  {pass}")
    };
    // `nostop` keeps printing; `noprint` implies `nostop`.
    for expected in [
        shown("SIGUSR1", "yes", "yes", "yes"),
        shown("SIGUSR1", "no", "yes", "yes"),
        shown("SIGALRM", "no", "yes", "no"),
        shown("SIG35", "no", "no", "yes"),
    ] {
        assert!(
            lines.contains(&expected.as_str()),
            "{expected:?}:\n{stdout}"
        );
    }
    // Received signals are reported in order before the program exits with
    // every handler but the discarded SIGALRM's.
    let usr1 = line_index(&lines, 0, |line| {
        line.starts_with("thread ")
            && line.contains(" received SIGUSR1 (SI_TKILL) sent by process ")
    });
    let alarm = line_index(&lines, usr1, |line| line.contains(" received SIGALRM "));
    let exited = line_index(&lines, alarm, |line| line.contains("exited with status 61"));
    assert!(exited > alarm);
}

#[test]
fn info_signals_lists_every_signals_policy() {
    let stdout = batch(&[BASIC], &["info signals"]);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "signal    stop  print  pass");
    assert_eq!(lines.len(), 65, "{stdout}");
    assert!(lines.contains(&"SIGINT    yes   yes    no"), "{stdout}");
    assert!(lines.contains(&"SIGURG    no    no     yes"), "{stdout}");
    assert!(lines.contains(&"SIG64     yes   yes    yes"), "{stdout}");
}

#[test]
fn the_terminal_launcher_explains_why_it_cannot_run_the_program() {
    let directory = support::ScratchDir::new("cli-launcher");
    let socket = directory.path().join("launcher");
    let output = uscope(&[
        "dap-launcher",
        "--connect",
        socket.to_str().expect("path"),
        "--",
        "/bin/true",
    ]);
    assert_eq!(output.status.code(), Some(127));
    assert_failure(
        &output,
        "uscope: cannot run /bin/true: No such file or directory",
    );

    // The adapter went away before it traced the launcher.
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
    let adapter = thread::spawn(move || drop(listener.accept().expect("accept")));
    let output = uscope(&[
        "dap-launcher",
        "--connect",
        socket.to_str().expect("path"),
        "--",
        "/bin/true",
    ]);
    adapter.join().expect("adapter");
    assert_eq!(output.status.code(), Some(127));
    assert_failure(
        &output,
        "uscope: cannot run /bin/true: the debugger stopped before it could debug it",
    );
}

/// A development build streams its flight recording as it goes, so even a
/// killed session leaves the requests, native calls, wait statuses, and
/// events that led up to the kill.
#[cfg(debug_assertions)]
#[test]
fn a_killed_session_leaves_its_flight_recording() {
    let scratch = support::ScratchDir::new("flight-recording");
    let recording = scratch.path().join("recording.log");
    // uscope keeps reading its open standard input after the script.
    let uscope = Uscope::spawn(
        Command::new(env!("CARGO_BIN_EXE_uscope"))
            .env("USCOPE_FLIGHT_RECORDING", &recording)
            .args(["--eval", "break main", "--eval", "run"])
            .arg(fixture("build/test-programs/basic")),
    );

    let mut contents = String::new();
    support::wait_until("the recording shows the stop", || {
        contents = fs::read_to_string(&recording).unwrap_or_default();
        contents.contains("event InferiorStopped")
    });
    // Dropping kills uscope and reaps it.
    drop(uscope);

    let position = |needle: &str| {
        contents
            .find(needle)
            .unwrap_or_else(|| panic!("the recording lacks {needle:?}:\n{contents}"))
    };
    let order = [
        "request add breakpoint",
        "request launch",
        "spawn ",
        "stopped by SIGTRAP",
        "install breakpoint",
        "PTRACE_CONT",
        "classified",
        "event InferiorStopped",
    ]
    .map(position);
    assert!(order.is_sorted(), "out of order {order:?}:\n{contents}");

    // The inferior dies with the debugger that traced it.
    let inferior = contents
        .lines()
        .find_map(|line| line.split_once("spawned ")?.1.parse::<u32>().ok())
        .expect("the recording names the inferior");
    // Once uscope is gone, init reaps the inferior, if it ever does.
    support::wait_until(&format!("inferior {inferior} dies with uscope"), || {
        matches!(process_state(inferior), None | Some('Z'))
    });
}

#[test]
fn expressions_print_compute_and_point_at_their_errors() {
    let (stdout, stderr) = piped(
        &["build/test-programs/expressions-c-gcc-o0-pie"],
        &[
            "break barrier",
            "run",
            "up",
            "print f.u8 + 10",
            "print (u8)(f.u8 + 10)",
            "print f.u32 * f.u32",
            "print/x f.i32",
            "print/x -1",
            "print f.arr[1..3]",
            "print f.head->next->value",
            "print gone + 1",
            "print f.arr[9]",
            "print 1 / 0",
            "print f.nope",
            "print (1 < 2 < 3)",
            "print \"é\" == f.zzz",
            "whatis f.u8 + 1",
            "whatis f.inner",
            "whatis (short)f.i32",
            "ptype struct inner",
            "ptype f.color",
            "print 017",
        ],
    );
    for expected in [
        "(integer) f.u8 + 10 = 260",
        "(u8) (u8)(f.u8 + 10) = 4",
        "(integer) f.u32 * f.u32 = 16000000000000000000",
        "(int32_t) f.i32 = 0xfffeee90",
        "(integer) -1 = -0x1",
        "20, 30",
        "(int) f.head->next->value = 2",
        "type = integer",
        "type = inner\n",
        "type = short int\n",
        "type = struct inner {\n    short int s;\n    long long int ll;\n}",
        "type = enum color {RED = 0, GREEN = 5, BLUE = 7}",
    ] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    for expected in [
        "no variable is named `gone` here\n    gone + 1\n    ^^^^\n",
        "index 9 is outside the source bounds starting at 0 with 5 elements\n    f.arr[9]\n    ^^^^^^^^\n",
        "division by zero\n    1 / 0\n    ^^^^^\n",
        "has no member named 'nope'\n    f.nope\n      ^^^^\n",
        "comparisons do not chain\n    (1 < 2 < 3)\n           ^\nhint: join comparisons with `&&`",
        "no member named 'zzz'\n    \"é\" == f.zzz\n             ^^^\n",
        "an integer cannot begin with 0\n    017\n    ^^^\nhint: write 0o17 for octal or 17 for decimal",
    ] {
        assert!(
            stderr.contains(expected),
            "missing `{expected}` in:\n{stderr}"
        );
    }
}

/// Code in the vDSO is shown as code in any library is: named by a symbol
/// when one covers it, and always by the module the kernel names `[vdso]`.
#[test]
fn vdso_code_is_shown_in_the_module_the_kernel_names() {
    let core = batch(
        &["--core", "build/test-programs/vdso-gcc-o0-clock.core"],
        &["info core", "bt", "where"],
    );
    let lines = core.lines().collect::<Vec<_>>();
    assert!(
        lines
            .iter()
            .any(|line| line.contains(" [vdso] module ") && line.ends_with(" read from the dump")),
        "{core}"
    );
    let innermost = lines
        .iter()
        .position(|line| line.starts_with("#0 "))
        .unwrap_or_else(|| panic!("{core}"));
    assert!(
        lines[innermost].ends_with(" in <unknown> from [vdso]"),
        "{core}"
    );
    assert!(
        lines[innermost + 1].contains(" in clock_gettime+0x")
            && lines[innermost + 1].contains(" from libc.so"),
        "{core}"
    );
    assert!(
        lines[innermost + 2].contains(" in vdso_clock at "),
        "{core}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("<unknown> at 0x") && line.ends_with(" from [vdso]")),
        "{core}"
    );

    let live = batch(
        &["build/test-programs/vdso-gcc-o0", "--", "time"],
        &["run", "bt"],
    );
    let innermost = live
        .lines()
        .find(|line| line.starts_with("#0 "))
        .unwrap_or_else(|| panic!("{live}"));
    assert!(
        (innermost.contains(" in time+0x") || innermost.contains(" in __vdso_time+0x"))
            && innermost.ends_with(" from [vdso]"),
        "{live}"
    );
}
