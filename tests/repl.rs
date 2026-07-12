use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use expectrl::session::{OsSession, Session};
use expectrl::{ControlCode, Eof, Expect};

struct TestStateDir(PathBuf);

impl TestStateDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "uscope-repl-{name}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test state directory");
        Self(path)
    }
}

impl Drop for TestStateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(name)
}

fn repl(executable: &Path, state: &Path) -> OsSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uscope"));
    command.arg(executable).env("XDG_STATE_HOME", state);
    let mut session = Session::spawn(command).expect("spawn uscope in a PTY");
    session.set_expect_timeout(Some(Duration::from_secs(2)));
    session.expect("> ").expect("initial prompt");
    session
}

#[test]
fn interactive_history_supports_arrows_control_navigation_and_reverse_search() {
    let state = TestStateDir::new("navigation");
    let mut session = repl(&fixture("basic"), &state.0);

    session.send_line("help quit").expect("send command");
    session.expect("aliases: q").expect("command output");
    session.expect("> ").expect("prompt after command");

    session.send(b"\x1b[A").expect("send up arrow");
    session.send_line("").expect("execute recalled command");
    session
        .expect("aliases: q")
        .expect("up arrow recalled history");
    session.expect("> ").expect("prompt after recalled command");

    session.send([0x10]).expect("send Ctrl-P");
    session.send_line("").expect("execute Ctrl-P command");
    session
        .expect("aliases: q")
        .expect("Ctrl-P recalled history");
    session.expect("> ").expect("prompt after Ctrl-P command");

    session.send(b"\x1b[A").expect("send up arrow");
    session.send(b"\x1b[B").expect("send down arrow");
    session.send("help quit").expect("type after Down");
    session.send_line("").expect("execute line after Down");
    session
        .expect("aliases: q")
        .expect("Down restored current line");
    session.expect("> ").expect("prompt after Down command");

    session.send([0x10]).expect("send Ctrl-P");
    session.send([0x0e]).expect("send Ctrl-N");
    session.send("help quit").expect("type after Ctrl-N");
    session.send_line("").expect("execute line after Ctrl-N");
    session
        .expect("aliases: q")
        .expect("Ctrl-N restored current line");
    session.expect("> ").expect("prompt after Ctrl-N command");

    session.send("quit").expect("type one word");
    session.send(b"\x1b[1;3D").expect("send Alt-Left");
    session.send("help ").expect("insert before prior word");
    session.send_line("").expect("execute Alt-Left edit");
    session
        .expect("aliases: q")
        .expect("Alt-Left moved by one word");
    session.expect("> ").expect("prompt after Alt-Left edit");

    session.send("help").expect("type one word");
    session.send([0x01]).expect("send Ctrl-A");
    session.send(b"\x1b[1;3C").expect("send Alt-Right");
    session.send(" quit").expect("insert after prior word");
    session.send_line("").expect("execute Alt-Right edit");
    session
        .expect("aliases: q")
        .expect("Alt-Right moved by one word");
    session.expect("> ").expect("prompt after Alt-Right edit");

    session.send([0x12]).expect("send Ctrl-R");
    session.send("help q").expect("type reverse-search query");
    session.send_line("").expect("accept reverse-search result");
    session
        .expect("aliases: q")
        .expect("reverse search found command");
    session.expect("> ").expect("prompt after reverse search");

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_control_c_cancels_the_line_and_control_l_redraws() {
    let state = TestStateDir::new("editing");
    let mut session = repl(&fixture("basic"), &state.0);

    session.send("invalid-command").expect("type invalid line");
    session.send(ControlCode::EndOfText).expect("send Ctrl-C");
    session.expect("> ").expect("Ctrl-C redrew prompt");

    session.send(ControlCode::FormFeed).expect("send Ctrl-L");
    session.expect("> ").expect("Ctrl-L redrew prompt");

    session
        .send(ControlCode::EndOfTransmission)
        .expect("send Ctrl-D");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_empty_lines_repeat_the_last_session_command() {
    let state = TestStateDir::new("repeat");
    let mut session = repl(&fixture("basic"), &state.0);

    session.send_line("").expect("send initial empty line");
    session
        .expect("> ")
        .expect("empty line before any command is a no-op");

    session.send_line("break main").expect("set breakpoint");
    session.expect("breakpoint set").expect("breakpoint reply");
    session.expect("> ").expect("prompt after breakpoint");

    session.send_line("run").expect("run inferior");
    session
        .expect("=>  9 | int main(void) {")
        .expect("main stop");
    session.expect("> ").expect("prompt after run");

    session.send_line("next").expect("send next");
    session.expect("=> 10 | ").expect("first source step");
    session
        .expect("> ")
        .expect("prompt after first source step");

    session.send_line("").expect("repeat next once");
    session.expect("=> 11 | ").expect("repeated source step");
    session
        .expect("> ")
        .expect("prompt after repeated source step");

    session.send_line("").expect("repeat next twice");
    session
        .expect("=> 12 | ")
        .expect("second repeated source step");
    session
        .expect("> ")
        .expect("prompt after second repeated source step");

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_history_persists_across_sessions() {
    let state = TestStateDir::new("persistence");
    {
        let mut session = repl(&fixture("basic"), &state.0);
        session.send_line("help quit").expect("record command");
        session.expect("> ").expect("prompt after command");
        session.send_line("quit").expect("quit first repl");
        session.expect(Eof).expect("first repl exited");
    }

    let mut session = repl(&fixture("basic"), &state.0);
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
