//! The interactive and scripted command-line client.
//!
//! The CLI is one client of [`DebuggerHandle`]: it parses commands, issues
//! requests, and renders replies. Debugger semantics live in the library.

mod commands;
mod format;
mod repl;
pub mod terminal;
mod value;

use std::ffi::OsString;
use std::fmt::Display;
use std::fs;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::ValueEnum;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use uscope::{AssemblySyntax, DebuggerEvent, DebuggerHandle, Error, LaunchOptions, StopReason};

use crate::Args;
use terminal::{
    ColorChoice, ColorEnvironment, Renderer, Role, color_enabled, terminal_control_enabled,
};

/// Renderers for each output stream.
#[derive(Clone, Copy)]
pub struct Renderers {
    pub stdout: Renderer,
    pub stderr: Renderer,
    /// Whether stdout accepts cursor-control sequences.
    pub stdout_control: bool,
}

impl Renderers {
    /// Detects color and terminal support from the streams and environment.
    pub fn detect(choice: ColorChoice, batch: bool) -> Self {
        let environment = ColorEnvironment::current();
        let stdout_is_terminal = io::stdout().is_terminal();
        let stderr_is_terminal = io::stderr().is_terminal();
        Self {
            stdout: Renderer::new(color_enabled(
                choice,
                &environment,
                stdout_is_terminal,
                batch,
            )),
            stderr: Renderer::new(color_enabled(
                choice,
                &environment,
                stderr_is_terminal,
                batch,
            )),
            stdout_control: terminal_control_enabled(&environment, stdout_is_terminal),
        }
    }
}

/// The assembly syntax `disassemble` renders.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum DisassemblySyntax {
    /// Intel syntax: destination first, `qword ptr [rbp-0x8]`.
    #[default]
    Intel,
    /// AT&T syntax as GNU tools print it: source first, `-0x8(%rbp)`.
    Att,
}

impl From<DisassemblySyntax> for AssemblySyntax {
    fn from(syntax: DisassemblySyntax) -> Self {
        match syntax {
            DisassemblySyntax::Intel => Self::Intel,
            DisassemblySyntax::Att => Self::Att,
        }
    }
}

/// What the input loop does after a command.
enum Control {
    /// Print the output, if any, and read the next command.
    Continue(String),
    ClearScreen,
    Quit,
}

/// How a script reacts to a failed command.
#[derive(Clone, Copy)]
enum OnError {
    /// Stop the script and fail the session.
    Abort,
    /// Report the error and read the next command.
    Report,
}

/// How `run` starts the inferior.
#[derive(Clone, Debug, Default)]
pub struct LaunchSettings {
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, Option<OsString>)>,
    pub working_directory: Option<PathBuf>,
}

impl LaunchSettings {
    /// The options for one launch, sharing the debugger's standard streams.
    fn options(&self) -> LaunchOptions {
        LaunchOptions {
            arguments: self.arguments.clone(),
            environment: self.environment.clone(),
            working_directory: self.working_directory.clone(),
            ..LaunchOptions::default()
        }
    }
}

pub struct Cli {
    debugger: DebuggerHandle,
    renderers: Renderers,
    syntax: AssemblySyntax,
    launch: LaunchSettings,
}

impl Cli {
    pub const fn new(
        debugger: DebuggerHandle,
        renderers: Renderers,
        syntax: AssemblySyntax,
        launch: LaunchSettings,
    ) -> Self {
        Self {
            debugger,
            renderers,
            syntax,
            launch,
        }
    }

    /// Runs the session's scripts and then its REPL, pausing the inferior on
    /// Ctrl-C.
    pub async fn run(&self, args: &Args) -> Result<()> {
        let session = self.run_inputs(args);
        tokio::pin!(session);
        loop {
            tokio::select! {
                result = &mut session => return result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl-C")?;
                    self.interrupt().await;
                }
            }
        }
    }

    /// Pauses a running inferior. Ctrl-C never ends the session: with nothing
    /// running there is nothing to interrupt, and `quit` or end-of-input exits.
    async fn interrupt(&self) {
        match self.debugger.pause().await {
            Ok(_) | Err(Error::NotRunning | Error::AlreadyStopped | Error::PostMortemTarget) => {}
            Err(error) => self.report_error(&anyhow!(error).context("failed to pause inferior")),
        }
    }

    async fn run_inputs(&self, args: &Args) -> Result<()> {
        self.announce(args)?;

        for path in &args.command_files {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("failed to read command file {}", path.display()))?;
            for (index, line) in contents.lines().enumerate() {
                let label = format!("{}:{}", path.display(), index + 1);
                if !self.run_scripted(line, label, OnError::Abort).await? {
                    return Ok(());
                }
            }
        }
        for (index, command) in args.commands.iter().enumerate() {
            let label = format!("--eval #{}", index + 1);
            if !self.run_scripted(command, label, OnError::Abort).await? {
                return Ok(());
            }
        }

        if !args.batch {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                return repl::run(self).await;
            }
            return self.run_stdin(OnError::Report).await;
        }
        if args.command_files.is_empty() && args.commands.is_empty() {
            return self.run_stdin(OnError::Abort).await;
        }
        Ok(())
    }

    fn announce(&self, args: &Args) -> Result<()> {
        let stdout = self.renderers.stdout;
        if let Some(core) = self.debugger.core_dump() {
            if !args.batch {
                emit(&format!(
                    "{} {} of {} (process {})\n{}",
                    stdout.paint(Role::Success, "opened core dump"),
                    stdout.paint(Role::Metadata, core.path.display()),
                    stdout.paint(Role::Name, &core.process_name),
                    core.process_id,
                    format::stop(
                        &StopReason::CoreDump {
                            exception: core.exception.clone()
                        },
                        stdout
                    )
                ))?;
            }
            // Modules not proven to match the dump are always reported.
            for warning in format::core_module_warnings(core) {
                self.warn(&warning);
            }
        } else if !args.batch {
            let action = if args.attach.is_some() {
                "attached to"
            } else {
                "debugging"
            };
            emit(&format!(
                "{} {}{}",
                stdout.paint(Role::Success, action),
                stdout.paint(Role::Metadata, self.debugger.executable().display()),
                args.attach
                    .map(|pid| format!(" (process {pid})"))
                    .unwrap_or_default()
            ))?;
        }
        Ok(())
    }

    async fn run_stdin(&self, on_error: OnError) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut number = 0_u64;
        while let Some(line) = lines.next_line().await? {
            number += 1;
            if !self
                .run_scripted(&line, format!("stdin:{number}"), on_error)
                .await?
            {
                break;
            }
        }
        Ok(())
    }

    /// Runs one line of a script and returns whether to keep reading.
    async fn run_scripted(
        &self,
        line: &str,
        label: impl Display,
        on_error: OnError,
    ) -> Result<bool> {
        match self.run_line(line).await {
            Ok(keep_running) => Ok(keep_running),
            Err(error) => {
                let error = error.context(label.to_string());
                match on_error {
                    OnError::Abort => Err(error),
                    OnError::Report if is_broken_pipe(&error) => Err(error),
                    OnError::Report => {
                        self.report_error(&error);
                        Ok(true)
                    }
                }
            }
        }
    }

    /// Executes one command line and returns whether to keep reading.
    async fn run_line(&self, line: &str) -> Result<bool> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(true);
        }
        match self.execute(line).await? {
            Control::Continue(output) => {
                if !output.is_empty() {
                    emit(&output)?;
                }
                Ok(true)
            }
            Control::ClearScreen => {
                if !self.renderers.stdout_control {
                    bail!("cannot clear screen: stdout is not an ANSI terminal");
                }
                let mut stdout = io::stdout().lock();
                stdout.write_all(b"\x1b[2J\x1b[H")?;
                stdout.flush()?;
                Ok(true)
            }
            Control::Quit => Ok(false),
        }
    }

    /// Waits for an execution request, prefixing its result with a line
    /// for each signal the inferior received without stopping.
    async fn report_signals(
        &self,
        execution: impl std::future::Future<Output = uscope::Result<StopReason>>,
    ) -> Result<(String, StopReason)> {
        let mut events = self.debugger.subscribe();
        let mut lines = Vec::new();
        let mut record = |event: Result<DebuggerEvent, _>| {
            if let Ok(DebuggerEvent::SignalReceived {
                thread_id,
                exception,
                ..
            }) = event
            {
                lines.push(format::signal_received(
                    thread_id,
                    &exception,
                    self.renderers.stdout,
                ));
            }
        };
        tokio::pin!(execution);
        let reason = loop {
            tokio::select! {
                biased;
                event = events.recv() => record(event),
                reason = &mut execution => break reason?,
            }
        };
        while let Ok(event) = events.try_recv() {
            record(Ok(event));
        }
        Ok((lines.join("\n"), reason))
    }

    fn report_error(&self, error: &anyhow::Error) {
        eprintln!(
            "{}: {error:#}",
            self.renderers.stderr.paint(Role::Error, "error")
        );
    }

    fn warn(&self, message: &str) {
        eprintln!(
            "{}: {message}",
            self.renderers.stderr.paint(Role::Warning, "warning")
        );
    }
}

/// Writes one line to stdout. Unlike `println!`, a closed pipe is an error
/// rather than a panic.
fn emit(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}")?;
    stdout.flush()
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
}
