//! What a Go fixture reports about itself, and a session that collects it.
//!
//! A fixture prints tab-separated `TRUTH` lines at each checkpoint, then
//! calls `main.reached`, where the session stops. The lines come from the
//! runtime's own goroutine dump, so they stay right when the toolchain
//! moves.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use uscope::{LaunchOptions, StopReason, TaskPage, TaskSnapshot};

use crate::support::{Scenario, ScratchDir};

/// One goroutine as the runtime's dump shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpedTask {
    /// Its status: the reason it waits, or a word such as `running`.
    pub status: String,
    /// Its frames, innermost first, without the runtime's own: each a
    /// function and its `file:line`.
    pub frames: Vec<(String, String)>,
}

/// What the program reported at one checkpoint.
#[derive(Debug, Default)]
pub struct Checkpoint {
    /// `runtime.NumGoroutine()`.
    pub count: usize,
    /// The goroutine that reached the checkpoint, and its thread.
    pub main: (u64, u64),
    /// Every goroutine the program started, by id.
    pub tasks: BTreeMap<u64, DumpedTask>,
}

impl Checkpoint {
    fn parse(lines: &[Vec<&str>]) -> Self {
        let mut checkpoint = Self::default();
        let number = |text: &str| text.parse::<u64>().expect("a number");
        for fields in lines {
            match fields.as_slice() {
                ["count", count] => checkpoint.count = count.parse().expect("a count"),
                ["main", id, thread] => checkpoint.main = (number(id), number(thread)),
                ["task", id, status] => {
                    checkpoint.tasks.insert(
                        number(id),
                        DumpedTask {
                            status: (*status).to_owned(),
                            frames: Vec::new(),
                        },
                    );
                }
                ["frame", id, function, position] => checkpoint
                    .tasks
                    .get_mut(&number(id))
                    .expect("a frame follows its task")
                    .frames
                    .push(((*function).to_owned(), (*position).to_owned())),
                _ => {}
            }
        }
        checkpoint
    }
}

/// A debugged Go program and what it printed.
pub struct GoSession {
    pub scenario: Scenario,
    pub fixture: String,
    output: PathBuf,
    _scratch: ScratchDir,
}

impl GoSession {
    /// Launches a fixture that stops at each of its checkpoints.
    pub async fn launch(fixture: &str) -> Self {
        let scratch = ScratchDir::new("go");
        let output = scratch.path().join("stdout");
        let file = std::fs::File::create(&output).expect("create the fixture's output");
        let errors = file.try_clone().expect("share the fixture's output");
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main.reached").await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::from(file)),
                stderr: Some(Stdio::from(errors)),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        Self {
            scenario,
            fixture: fixture.to_owned(),
            output,
            _scratch: scratch,
        }
    }

    /// What the program reported at its latest checkpoint, which must be
    /// `name`.
    pub fn checkpoint(&self, name: &str) -> Checkpoint {
        let printed = std::fs::read_to_string(&self.output).expect("read the fixture's output");
        let lines = printed
            .lines()
            .filter_map(|line| line.strip_prefix("TRUTH\t"))
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let start = lines
            .iter()
            .rposition(|fields| fields.first() == Some(&"checkpoint"))
            .unwrap_or_else(|| panic!("{}: no checkpoint in {printed}", self.fixture));
        assert_eq!(lines[start].get(1), Some(&name), "{}", self.fixture);
        Checkpoint::parse(&lines[start + 1..])
    }

    /// Every task of the stopped program, read a page at a time.
    pub async fn tasks(&self, page: usize) -> (Vec<TaskSnapshot>, Vec<String>) {
        let mut tasks = Vec::new();
        let mut gaps = Vec::new();
        let mut from = None;
        loop {
            let TaskPage {
                tasks: found,
                next,
                gaps: missing,
            } = self
                .scenario
                .operation("tasks", self.scenario.handle().tasks(from, page))
                .await;
            assert!(found.len() <= page, "{}", self.fixture);
            tasks.extend(found.iter().cloned());
            gaps.extend(missing.iter().map(ToString::to_string));
            match next {
                Some(next) => from = Some(next),
                None => return (tasks, gaps),
            }
        }
    }
}
