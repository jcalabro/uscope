//! The interactive and scripted command-line client.
//!
//! The CLI is one client of [`DebuggerHandle`]: it parses commands, issues
//! requests, and renders replies. Debugger semantics live in the library.

pub mod commands;
pub mod config;
pub mod format;
pub mod help;
mod repl;
pub mod session;
mod suggest;
pub mod terminal;
pub mod value;

use std::ffi::OsString;
use std::fmt::Display;
use std::fs;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::ValueEnum;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use uscope::{AssemblySyntax, DebuggerHandle, Error, LaunchOptions, StopReason};

use config::{Settings, Toggle};
use session::Session;
use terminal::{
    ColorChoice, ColorEnvironment, Look, Palette, Renderer, Role, color_enabled,
    terminal_control_enabled,
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

    /// Renderers for a debug adapter's console: paths relative to the
    /// project root, as the CLI shows them, and Unicode, which every editor
    /// shows.
    pub fn console(color: bool, root: PathBuf) -> Self {
        // One look per session, which lives as long as the adapter.
        let look: &'static Look = Box::leak(Box::new(Look {
            palette: Palette::new(
                terminal::ThemeName::Default,
                &terminal::ThemeOverrides::default(),
            ),
            paths: terminal::PathStyle::Relative,
            root: Some(root),
            unicode: true,
            hyperlinks: false,
        }));
        Self {
            stdout: Renderer::with_look(color, look),
            stderr: Renderer::with_look(color, look),
            stdout_control: false,
        }
    }

    /// Renderers in the look the settings describe, colored as they and
    /// the streams allow.
    pub fn configured(settings: &Settings, batch: bool) -> Self {
        let ui = &settings.config.ui;
        let detected = Self::detect(ui.color, batch);
        let stdout_is_terminal = io::stdout().is_terminal();
        let look: &'static Look = Box::leak(Box::new(Look {
            palette: Palette::new(ui.theme, &settings.config.theme),
            paths: ui.paths,
            root: Some(settings.root.clone()),
            unicode: match ui.unicode {
                Toggle::Always => true,
                Toggle::Never => false,
                Toggle::Auto => locale_is_utf8(),
            },
            hyperlinks: match ui.hyperlinks {
                Toggle::Always => true,
                Toggle::Never => false,
                Toggle::Auto => !batch && stdout_is_terminal && terminal_has_hyperlinks(),
            },
        }));
        Self {
            stdout: Renderer::with_look(detected.stdout.is_colored(), look),
            stderr: Renderer::with_look(detected.stderr.is_colored(), look),
            stdout_control: detected.stdout_control,
        }
    }
}

/// Whether the locale's character set is UTF-8, by the first of `LC_ALL`,
/// `LC_CTYPE`, and `LANG` that is set.
fn locale_is_utf8() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        })
}

/// Whether the terminal is one known to show OSC 8 hyperlinks.
fn terminal_has_hyperlinks() -> bool {
    let variable = |name| std::env::var(name).unwrap_or_default();
    let program = variable("TERM_PROGRAM");
    let term = variable("TERM");
    matches!(
        program.as_str(),
        "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "Hyper" | "rio"
    ) || ["kitty", "foot", "alacritty", "ghostty", "wezterm"]
        .iter()
        .any(|name| term.contains(name))
        || std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || variable("VTE_VERSION")
            .parse::<u32>()
            .is_ok_and(|version| version >= 5000)
}

/// The assembly syntax `disassemble` renders.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
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

impl From<AssemblySyntax> for DisassemblySyntax {
    fn from(syntax: AssemblySyntax) -> Self {
        match syntax {
            AssemblySyntax::Intel => Self::Intel,
            AssemblySyntax::Att => Self::Att,
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
    settings: Settings,
    syntax: AssemblySyntax,
    launch: LaunchSettings,
    views: std::sync::Mutex<ViewSources>,
}

/// The view files a session loads: those it was given or loaded, most
/// recent first, then the project's and the user's.
#[derive(Default)]
struct ViewSources {
    session: Vec<uscope::view_files::ViewFile>,
    discovered: Vec<uscope::view_files::ViewFile>,
}

impl Cli {
    pub fn new(
        debugger: DebuggerHandle,
        renderers: Renderers,
        settings: Settings,
        launch: LaunchSettings,
    ) -> Self {
        Self {
            debugger,
            renderers,
            syntax: settings.config.disassembly.syntax.into(),
            settings,
            launch,
            views: std::sync::Mutex::new(ViewSources {
                session: Vec::new(),
                discovered: Vec::new(),
            }),
        }
    }

    /// Loads the project's view files, under `project_root`, and the
    /// user's, and the session files at `paths`, and returns a warning for
    /// each file or view it could not use, the program's own included.
    pub async fn load_view_sources(
        &self,
        project_root: &std::path::Path,
        paths: &[PathBuf],
    ) -> Vec<String> {
        let (discovered, mut warnings) = uscope::view_files::discover(project_root);
        let mut session = Vec::new();
        for path in paths {
            match uscope::view_files::read(path) {
                Ok(file) => session.insert(0, file),
                Err(error) => warnings.push(error),
            }
        }
        {
            let mut views = self.views.lock().expect("the view sources are whole");
            views.discovered = discovered;
            views.session = session;
        }
        warnings.extend(self.reload_views().await);
        warnings.extend(
            self.debugger
                .module_image()
                .view_errors()
                .iter()
                .map(ToString::to_string),
        );
        warnings
    }

    /// Loads view files as [`Self::load_view_sources`] does and warns about
    /// each one that cannot be used. Returns whether every one could be used.
    pub async fn load_views(&self, project_root: &std::path::Path, paths: &[PathBuf]) -> bool {
        let warnings = self.load_view_sources(project_root, paths).await;
        for warning in &warnings {
            self.warn(&format!("views: {warning}"));
        }
        warnings.is_empty()
    }

    /// Presents values with the session's view files and the kernels beside
    /// them, and returns what kept parts of them out.
    async fn reload_views(&self) -> Vec<String> {
        let sources = {
            let views = self.views.lock().expect("the view sources are whole");
            views
                .session
                .iter()
                .chain(&views.discovered)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut kernels = Vec::<uscope::view_files::KernelFile>::new();
        // Files in one directory share the kernels beside them.
        for kernel in sources.iter().flat_map(|file| &file.kernels) {
            if !kernels.iter().any(|loaded| loaded.path == kernel.path) {
                kernels.push(kernel.clone());
            }
        }
        let files = sources
            .iter()
            .map(|file| (file.name.as_str(), file.text.as_str()))
            .collect::<Vec<_>>();
        match self.debugger.load_views(&files, &kernels).await {
            Ok(errors) => errors.iter().map(ToString::to_string).collect(),
            Err(error) => vec![error.to_string()],
        }
    }

    /// Runs the session's scripts and then its REPL, pausing the inferior on
    /// Ctrl-C.
    pub async fn run(&self, session: &Session) -> Result<()> {
        let session = self.run_inputs(session);
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

    async fn run_inputs(&self, args: &Session) -> Result<()> {
        self.announce(args)?;
        self.load_views(&self.settings.root, &args.views).await;
        self.apply_signal_settings().await?;

        for command in &args.startup {
            if !self
                .run_scripted(&command.text, &command.label, OnError::Abort)
                .await?
            {
                return Ok(());
            }
        }
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

    /// Applies the settings' `[signals]`, as `handle` would.
    async fn apply_signal_settings(&self) -> Result<()> {
        for (name, actions) in &self.settings.config.signals {
            let code = uscope::signal_named(name).expect("checked when read");
            let mut policy = self.debugger.signal_policy(code).await?;
            for action in actions.actions() {
                commands::apply_signal_action(&mut policy, action)?;
            }
            self.debugger.set_signal_policy(code, policy).await?;
        }
        Ok(())
    }

    fn announce(&self, args: &Session) -> Result<()> {
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
            let action = if args.attached().is_some() {
                "attached to"
            } else {
                "debugging"
            };
            emit(&format!(
                "{} {}{}",
                stdout.paint(Role::Success, action),
                stdout.paint(Role::Metadata, self.debugger.executable().display()),
                args.attached()
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

pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
}
