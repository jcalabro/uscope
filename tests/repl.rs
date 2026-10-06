mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use expectrl::session::{OsSession, Session};
use expectrl::{ControlCode, Eof, Expect};

/// The harness's deadline for anything a test waits to observe.
const TIMEOUT: Duration = Duration::from_secs(5);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(name)
}

/// A uscope command for a terminal that supports color, with history kept
/// under `state` and no color settings in the environment.
fn command(executable: &Path, state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uscope"));
    command
        .arg(executable)
        .env("XDG_STATE_HOME", state)
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn spawn(command: Command) -> OsSession {
    let mut session = Session::spawn(command).expect("spawn uscope in a PTY");
    session.set_expect_timeout(Some(TIMEOUT));
    session
}

/// Starts an uncolored REPL and waits for its first prompt.
fn repl(executable: &Path, state: &Path) -> OsSession {
    let mut command = command(executable, state);
    command.args(["--color", "never"]);
    let mut session = spawn(command);
    session.expect("(uscope) ").expect("initial prompt");
    session
}

fn contains_sgr(bytes: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(start) = bytes[offset..].windows(2).position(|pair| pair == b"\x1b[") {
        let sequence = &bytes[offset + start + 2..];
        if let Some(final_byte) = sequence.iter().find(|&&byte| (0x40..=0x7e).contains(&byte))
            && *final_byte == b'm'
        {
            return true;
        }
        offset += start + 2;
    }
    false
}

#[test]
fn interactive_named_prompt_preserves_rustyline_cursor_width() {
    let state = support::ScratchDir::new("repl-color");
    let mut session = spawn(command(&fixture("basic"), state.path()));
    session
        .expect("\x1b[2m(uscope) \x1b[0m")
        .expect("dimmed prompt");

    session.send("quit").expect("type command");
    session.send([0x01]).expect("send Ctrl-A");
    session.send(b"help ").expect("insert at prompt start");
    session.send_line("").expect("execute edited command");
    session.expect("aliases").expect("edited command output");
    session.expect("(uscope) ").expect("redrawn prompt");
    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_prompt_honors_no_color() {
    let state = support::ScratchDir::new("repl-no-color");
    let mut command = command(&fixture("basic"), state.path());
    command.env("NO_COLOR", "1");
    let mut session = spawn(command);
    let prompt = session.expect("(uscope) ").expect("plain prompt");
    assert!(
        !contains_sgr(prompt.as_bytes()),
        "NO_COLOR prompt contained color: {:?}",
        prompt.as_bytes()
    );

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_control_c_cancels_the_line_and_control_l_redraws() {
    let state = support::ScratchDir::new("repl-editing");
    let mut session = repl(&fixture("basic"), state.path());

    session.send("invalid-command").expect("type invalid line");
    session.send(ControlCode::EndOfText).expect("send Ctrl-C");
    session.expect("(uscope) ").expect("Ctrl-C redrew prompt");

    session.send(ControlCode::FormFeed).expect("send Ctrl-L");
    session.expect("(uscope) ").expect("Ctrl-L redrew prompt");

    session
        .send(ControlCode::EndOfTransmission)
        .expect("send Ctrl-D");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_errors_omit_repl_context() {
    let state = support::ScratchDir::new("repl-errors");
    let mut session = repl(&fixture("basic"), state.path());

    session
        .send_line("break asdf")
        .expect("send invalid breakpoint");
    session
        .expect("error: no function named 'asdf' was found")
        .expect("concise interactive error");
    session.expect("(uscope) ").expect("prompt after error");

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_clear_command_and_cls_alias_clear_the_terminal() {
    let state = support::ScratchDir::new("repl-clear");
    let mut session = repl(&fixture("basic"), state.path());

    for command in ["clear", "cls"] {
        session.send_line(command).expect("send clear command");
        session
            .expect("\x1b[2J\x1b[H")
            .expect("terminal clear sequence");
        session.expect("(uscope) ").expect("prompt after clear");
    }

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_empty_lines_repeat_the_last_session_command() {
    let state = support::ScratchDir::new("repl-repeat");
    let mut session = repl(&fixture("basic"), state.path());

    session.send_line("").expect("send initial empty line");
    session
        .expect("(uscope) ")
        .expect("empty line before any command is a no-op");

    session.send_line("break main").expect("set breakpoint");
    session
        .expect("breakpoint 1 set")
        .expect("breakpoint reply");
    session
        .expect("(uscope) ")
        .expect("prompt after breakpoint");

    session.send_line("run").expect("run inferior");
    session.expect("=> 10 | ").expect("main stop");
    session.expect("(uscope) ").expect("prompt after run");

    session.send_line("next").expect("send next");
    session.expect("=> 11 | ").expect("first source step");
    session
        .expect("(uscope) ")
        .expect("prompt after first source step");

    for line in [12, 13] {
        session.send_line("").expect("repeat next");
        session
            .expect(format!("=> {line} | ").as_str())
            .expect("repeated source step");
        session
            .expect("(uscope) ")
            .expect("prompt after repeated source step");
    }

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_history_persists_across_sessions() {
    let state = support::ScratchDir::new("repl-history");
    {
        let mut session = repl(&fixture("basic"), state.path());
        session.send_line("help quit").expect("record command");
        session.expect("(uscope) ").expect("prompt after command");
        session.send_line("quit").expect("quit first repl");
        session.expect(Eof).expect("first repl exited");
    }

    let mut session = repl(&fixture("basic"), state.path());
    session.send(b"\x1b[A").expect("send up arrow");
    session
        .send(b"\x1b[A")
        .expect("skip persisted quit command");
    session.send_line("").expect("execute persisted command");
    session
        .expect("aliases: q")
        .expect("history was loaded in second session");
    session.send_line("quit").expect("quit second repl");
    session.expect(Eof).expect("second repl exited");
}
