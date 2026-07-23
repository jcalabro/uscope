use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::{env, thread};

use anyhow::{Context, Result};
use clap::Parser;
use rustc_apfloat::Float as _;
use rustc_apfloat::ieee::X87DoubleExtended;
use rustyline::config::Configurer as _;
use rustyline::error::ReadlineError;
use rustyline::{ColorMode, DefaultEditor};
use tokio::io::{AsyncBufReadExt, BufReader};
use uscope::{
    Breakpoint, BreakpointId, BreakpointLocation, BreakpointSpec, ByteOrder, Debugger,
    DebuggerHandle, Error, ExitStatus, FloatValue, LineNumber, RegisterSnapshot, ScalarValue,
    SourceContext, StateSnapshot, StepKind, StopReason, ThreadId, ThreadState, ValueExpression,
    Variable, VariableSnapshot, VariableState, VirtualAddress,
};

mod terminal;

use terminal::{
    ColorChoice, ColorEnvironment, Renderer, Role, color_enabled, terminal_control_enabled,
};

const REPL_PROMPT: &str = "(uscope) ";

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Native executable to debug.
    #[arg(value_name = "EXECUTABLE")]
    executable: PathBuf,

    /// Execute commands from a file. May be repeated.
    #[arg(short = 'c', long = "command", value_name = "FILE")]
    command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(short = 'e', long = "eval", value_name = "COMMAND")]
    commands: Vec<String>,

    /// Execute commands without starting the interactive REPL.
    #[arg(long)]
    batch: bool,

    /// Control colored terminal output.
    #[arg(long, value_enum, default_value_t)]
    color: ColorChoice,
}

#[derive(Clone, Copy)]
struct Renderers {
    stdout: Renderer,
    stderr: Renderer,
    stdout_control: bool,
}

#[derive(Clone, Copy)]
struct CommandSpec {
    command: Command,
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    summary: &'static str,
}

#[derive(Clone, Copy)]
enum Command {
    Break,
    Breakpoints,
    Info,
    Delete,
    Run,
    Continue,
    Pause,
    Print,
    Globals,
    Stepi,
    Step,
    Next,
    Finish,
    Examine,
    Address,
    Where,
    List,
    Backtrace,
    Registers,
    Threads,
    Thread,
    Clear,
    Help,
    Quit,
}

macro_rules! command {
    ($command:ident, $name:literal, [$($alias:literal),*], $usage:literal, $summary:literal) => {
        CommandSpec { command: Command::$command, name: $name, aliases: &[$($alias),*], usage: $usage, summary: $summary }
    };
}

const COMMANDS: &[CommandSpec] = &[
    command!(
        Break,
        "break",
        ["b"],
        "break <function|address|file:line|file:function>",
        "Set a breakpoint"
    ),
    command!(
        Breakpoints,
        "breakpoints",
        [],
        "breakpoints",
        "List logical breakpoints"
    ),
    command!(
        Info,
        "info",
        [],
        "info breakpoints",
        "Show debugger information"
    ),
    command!(
        Delete,
        "delete",
        ["del", "d"],
        "delete <id|all>",
        "Delete logical breakpoints"
    ),
    command!(Run, "run", ["r"], "run", "Launch the inferior"),
    command!(
        Continue,
        "continue",
        ["c"],
        "continue",
        "Continue execution"
    ),
    command!(Pause, "pause", [], "pause", "Pause execution"),
    command!(
        Print,
        "print",
        ["p"],
        "print [*...variable[.member...]]",
        "Print one or all visible variables, selecting members through pointers"
    ),
    command!(
        Globals,
        "globals",
        [],
        "globals [filter]",
        "List global variable metadata"
    ),
    command!(Stepi, "stepi", ["si"], "stepi", "Step one instruction"),
    command!(Step, "step", ["s"], "step", "Step into at source level"),
    command!(Next, "next", ["n"], "next", "Step over at source level"),
    command!(
        Finish,
        "finish",
        ["fin", "f"],
        "finish",
        "Run until the selected frame returns"
    ),
    command!(
        Examine,
        "x",
        [],
        "x <runtime-address>",
        "Examine one native word"
    ),
    command!(
        Address,
        "address",
        [],
        "address <symbol>",
        "Resolve a symbol's runtime address"
    ),
    command!(
        Where,
        "where",
        [],
        "where",
        "Show the current execution location"
    ),
    command!(
        List,
        "list",
        ["l"],
        "list",
        "Show source around the current location"
    ),
    command!(
        Backtrace,
        "backtrace",
        ["bt"],
        "backtrace",
        "Show the selected thread's stack"
    ),
    command!(
        Registers,
        "registers",
        ["regs"],
        "registers",
        "Show native registers"
    ),
    command!(Threads, "threads", [], "threads", "List threads"),
    command!(Thread, "thread", [], "thread <id>", "Select a thread"),
    command!(
        Clear,
        "clear",
        ["cls"],
        "clear",
        "Clear and redraw the terminal"
    ),
    command!(
        Help,
        "help",
        ["h", "?"],
        "help [command]",
        "Show command help"
    ),
    command!(Quit, "quit", ["q"], "quit", "Exit uscope"),
];

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let environment = ColorEnvironment::current();
    let stdout_is_terminal = io::stdout().is_terminal();
    let renderers = Renderers {
        stdout: Renderer::new(color_enabled(
            args.color,
            &environment,
            stdout_is_terminal,
            args.batch,
        )),
        stderr: Renderer::new(color_enabled(
            args.color,
            &environment,
            io::stderr().is_terminal(),
            args.batch,
        )),
        stdout_control: terminal_control_enabled(&environment, stdout_is_terminal),
    };

    match run_debugger(&args, renderers).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}: {error:#}",
                renderers.stderr.paint(Role::Error, "error")
            );
            ExitCode::FAILURE
        }
    }
}

async fn run_debugger(args: &Args, renderers: Renderers) -> Result<()> {
    let debugger = Debugger::new(&args.executable).with_context(|| {
        format!(
            "failed to initialize debugger for {}",
            args.executable.display()
        )
    })?;

    let handle = debugger.handle();
    let result = run_with_interrupts(&handle, args, renderers).await;
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down debugger");

    result?;
    shutdown?;

    Ok(())
}

async fn run_with_interrupts(
    debugger: &DebuggerHandle,
    args: &Args,
    renderers: Renderers,
) -> Result<()> {
    let mut terminal = Box::pin(run(debugger, args, renderers));

    loop {
        tokio::select! {
            result = &mut terminal => return result,
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for Ctrl-C")?;
                match debugger.pause().await {
                    Ok(_) => {}
                    Err(Error::NotRunning | Error::NotStopped) => return Ok(()),
                    Err(error) => return Err(error).context("failed to pause inferior"),
                }
            }
        }
    }
}

async fn run(debugger: &DebuggerHandle, args: &Args, renderers: Renderers) -> Result<()> {
    if !args.batch {
        println!(
            "{} {}",
            renderers.stdout.paint(Role::Success, "debugging"),
            renderers
                .stdout
                .paint(Role::Metadata, debugger.executable().display())
        );
        io::stdout().flush()?;
    }

    for path in &args.command_files {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read command file {}", path.display()))?;

        if !run_lines(
            debugger,
            contents.lines(),
            &path.display().to_string(),
            renderers.stdout,
            renderers.stdout_control,
        )
        .await?
        {
            return Ok(());
        }
    }

    for (index, command) in args.commands.iter().enumerate() {
        if !run_line(
            debugger,
            command,
            Some(&format!("--eval #{}", index + 1)),
            renderers.stdout,
            renderers.stdout_control,
        )
        .await?
        {
            return Ok(());
        }
    }

    if args.batch {
        if args.command_files.is_empty() && args.commands.is_empty() {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            let mut number = 0_u64;

            while let Some(line) = lines.next_line().await? {
                number = number.checked_add(1).expect("stdin line number overflow");

                if !run_line(
                    debugger,
                    &line,
                    Some(&format!("stdin:{number}")),
                    renderers.stdout,
                    renderers.stdout_control,
                )
                .await?
                {
                    break;
                }
            }
        }

        Ok(())
    } else {
        repl(debugger, renderers).await
    }
}

async fn run_lines<'a>(
    debugger: &DebuggerHandle,
    lines: impl Iterator<Item = &'a str>,
    source: &str,
    renderer: Renderer,
    terminal_control: bool,
) -> Result<bool> {
    for (index, line) in lines.enumerate() {
        if !run_line(
            debugger,
            line,
            Some(&format!("{source}:{}", index + 1)),
            renderer,
            terminal_control,
        )
        .await?
        {
            return Ok(false);
        }
    }

    Ok(true)
}

async fn run_line(
    debugger: &DebuggerHandle,
    line: &str,
    source: Option<&str>,
    renderer: Renderer,
    terminal_control: bool,
) -> Result<bool> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(true);
    }

    let control = execute(debugger, line, renderer).await;
    let control = match source {
        Some(source) => control.with_context(|| source.to_owned())?,
        None => control?,
    };
    match control {
        Control::Continue(message) => {
            if !message.is_empty() {
                println!("{message}");
                io::stdout().flush()?;
            }

            Ok(true)
        }
        Control::ClearScreen => {
            if !terminal_control {
                anyhow::bail!("cannot clear screen: stdout is not an ANSI terminal");
            }
            print!("\x1b[2J\x1b[H");
            io::stdout().flush()?;
            Ok(true)
        }
        Control::Quit => Ok(false),
    }
}

async fn repl(debugger: &DebuggerHandle, renderers: Renderers) -> Result<()> {
    let show_prompt = io::stdin().is_terminal() && io::stdout().is_terminal();
    if show_prompt {
        return interactive_repl(debugger, renderers).await;
    }
    stream_repl(debugger, false, renderers).await
}

async fn stream_repl(
    debugger: &DebuggerHandle,
    show_prompt: bool,
    renderers: Renderers,
) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut number = 0_u64;

    loop {
        if show_prompt {
            print!("{REPL_PROMPT}");
            io::stdout().flush()?;
        }

        let Some(line) = lines.next_line().await? else {
            if show_prompt {
                println!();
            }
            return Ok(());
        };
        number = number.checked_add(1).expect("REPL line number overflow");

        match run_line(
            debugger,
            &line,
            Some(&format!("stdin:{number}")),
            renderers.stdout,
            renderers.stdout_control,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => eprintln!(
                "{}: {error:#}",
                renderers.stderr.paint(Role::Error, "error")
            ),
        }
    }
}

enum ReplInput {
    Line(String),
    Eof,
    Failed(String),
}

enum ReplAck {
    Continue,
    Quit,
}

async fn interactive_repl(debugger: &DebuggerHandle, renderers: Renderers) -> Result<()> {
    let (input_sender, mut input_receiver) = tokio::sync::mpsc::channel(1);
    let (ack_sender, ack_receiver) = mpsc::channel();
    let editor = thread::Builder::new()
        .name("uscope-line-editor".to_owned())
        .spawn(move || line_editor(&input_sender, &ack_receiver, renderers))?;

    let mut outcome = Ok(());
    let mut last_command = None;
    while let Some(input) = input_receiver.recv().await {
        let keep_running = match input {
            ReplInput::Line(text) => {
                let trimmed = text.trim();
                let command = if trimmed.is_empty() {
                    last_command.as_deref().unwrap_or(trimmed)
                } else {
                    if !trimmed.starts_with('#') {
                        last_command = Some(trimmed.to_owned());
                    }
                    trimmed
                };
                match run_line(
                    debugger,
                    command,
                    None,
                    renderers.stdout,
                    renderers.stdout_control,
                )
                .await
                {
                    Ok(keep_running) => keep_running,
                    Err(error) => {
                        eprintln!(
                            "{}: {error:#}",
                            renderers.stderr.paint(Role::Error, "error")
                        );
                        true
                    }
                }
            }
            ReplInput::Eof => false,
            ReplInput::Failed(error) => {
                outcome = Err(anyhow::anyhow!(error));
                break;
            }
        };
        if ack_sender
            .send(if keep_running {
                ReplAck::Continue
            } else {
                ReplAck::Quit
            })
            .is_err()
        {
            outcome = Err(anyhow::anyhow!(
                "line editor stopped before command acknowledgement"
            ));
            break;
        }
        if !keep_running {
            break;
        }
    }

    tokio::task::spawn_blocking(move || editor.join())
        .await
        .context("failed to join line editor task")?
        .map_err(|_| anyhow::anyhow!("line editor thread panicked"))?;
    outcome
}

fn line_editor(
    input: &tokio::sync::mpsc::Sender<ReplInput>,
    acknowledgements: &mpsc::Receiver<ReplAck>,
    renderers: Renderers,
) {
    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(error) => {
            let _ = input.blocking_send(ReplInput::Failed(error.to_string()));
            return;
        }
    };
    // Rustyline needs a helper to select the styled prompt. The unit helper's
    // line highlighter is a no-op, so command input remains unstyled.
    editor.set_helper(Some(()));
    editor.set_color_mode(if renderers.stdout.is_colored() {
        ColorMode::Forced
    } else {
        ColorMode::Disabled
    });
    let history = history_path();
    if history.exists()
        && let Err(error) = editor.load_history(&history)
    {
        eprintln!(
            "{}: failed to load command history {}: {error}",
            renderers.stderr.paint(Role::Warning, "warning"),
            history.display()
        );
    }
    loop {
        let styled_prompt = renderers
            .stdout
            .paint(Role::Prompt, REPL_PROMPT)
            .to_string();
        match editor.readline(&(REPL_PROMPT, &styled_prompt)) {
            Ok(line) => {
                if !line.trim().is_empty()
                    && let Err(error) = editor.add_history_entry(line.as_str())
                {
                    eprintln!(
                        "{}: failed to record command history: {error}",
                        renderers.stderr.paint(Role::Warning, "warning")
                    );
                }
                if input.blocking_send(ReplInput::Line(line)).is_err()
                    || matches!(acknowledgements.recv(), Ok(ReplAck::Quit) | Err(_))
                {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {}
            Err(ReadlineError::Eof) => {
                let _ = input.blocking_send(ReplInput::Eof);
                let _ = acknowledgements.recv();
                break;
            }
            Err(error) => {
                let _ = input.blocking_send(ReplInput::Failed(error.to_string()));
                break;
            }
        }
    }
    persist_history(&mut editor, &history, renderers.stderr);
}

fn history_path() -> PathBuf {
    history_path_from(
        env::var_os("XDG_STATE_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

fn history_path_from(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    xdg_state_home.map_or_else(
        || {
            home.map_or_else(
                || PathBuf::from(".uscope_history"),
                |home| PathBuf::from(home).join(".local/state/uscope/history"),
            )
        },
        |state| PathBuf::from(state).join("uscope/history"),
    )
}

fn persist_history(editor: &mut DefaultEditor, path: &std::path::Path, renderer: Renderer) {
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!(
            "{}: failed to create history directory {}: {error}",
            renderer.paint(Role::Warning, "warning"),
            parent.display()
        );
        return;
    }
    let result = if path.exists() {
        editor.append_history(path)
    } else {
        editor.save_history(path)
    };
    if let Err(error) = result {
        eprintln!(
            "{}: failed to save command history {}: {error}",
            renderer.paint(Role::Warning, "warning"),
            path.display()
        );
    }
}

enum Control {
    Continue(String),
    ClearScreen,
    Quit,
}

async fn execute(
    debugger: &DebuggerHandle,
    line: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let mut words = line.split_whitespace();
    let entered = words.next().unwrap_or("");
    if entered.is_empty() {
        return Ok(Control::Continue(String::new()));
    }
    let spec = command_named(entered).ok_or_else(|| Error::InvalidCommand(entered.to_owned()))?;
    if spec.usage == spec.name {
        no_arguments(&mut words, spec.usage)?;
    }

    match spec.command {
        Command::Break => {
            let argument = one_argument(&mut words, spec.usage)?;
            execute_break(debugger, argument, renderer).await
        }
        Command::Breakpoints => execute_list_breakpoints(debugger, renderer).await,
        Command::Info => {
            let argument = one_argument(&mut words, spec.usage)?;
            if argument != "breakpoints" && argument != "break" {
                return Err(Error::InvalidCommand(spec.usage.to_owned()));
            }
            execute_list_breakpoints(debugger, renderer).await
        }
        Command::Delete => {
            let argument = one_argument(&mut words, spec.usage)?;
            execute_delete_breakpoint(debugger, argument, spec.usage, renderer).await
        }
        Command::Run => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.run().await?, renderer).await,
        )),
        Command::Continue => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.resume().await?, renderer).await,
        )),
        Command::Pause => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.pause().await?, renderer).await,
        )),
        Command::Print => execute_print(debugger, &mut words, spec.usage, renderer).await,
        Command::Globals => execute_globals(debugger, &mut words, spec.usage, renderer).await,
        Command::Stepi => execute_step(debugger, StepKind::Instruction, renderer).await,
        Command::Step => execute_step(debugger, StepKind::IntoSource, renderer).await,
        Command::Next => execute_step(debugger, StepKind::OverSource, renderer).await,
        Command::Finish => execute_step(debugger, StepKind::Out, renderer).await,
        Command::Examine => execute_examine(debugger, &mut words, spec.usage, renderer).await,
        Command::Address => execute_address(debugger, &mut words, spec.usage, renderer).await,
        Command::Where => execute_where(debugger, renderer).await,
        Command::List => Ok(Control::Continue(format_source_context(
            &debugger.source_context(3).await?,
            renderer,
        ))),
        Command::Backtrace => format_backtrace(debugger, renderer).await,
        Command::Registers => {
            let registers = debugger.registers().await?;

            Ok(Control::Continue(format_registers(
                &registers,
                registers.target.byte_order,
                renderer,
            )))
        }
        Command::Threads => {
            let snapshot = debugger.snapshot().await?;
            Ok(Control::Continue(format_threads(&snapshot, renderer)))
        }
        Command::Thread => select_thread(debugger, &mut words, spec.usage, renderer).await,
        Command::Clear => Ok(Control::ClearScreen),
        Command::Help => execute_help(&mut words, spec.usage, renderer),
        Command::Quit => Ok(Control::Quit),
    }
}

fn command_named(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
}

async fn execute_examine<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let address = parse_address(one_argument(words, usage)?)?;
    let value = debugger.read_word(VirtualAddress::new(address)).await?;
    Ok(Control::Continue(format!(
        "{}: {}",
        renderer.paint(Role::Metadata, format_args!("{address:#018x}")),
        renderer.paint(Role::Value, format_args!("{value:#018x}"))
    )))
}

async fn execute_address<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let name = one_argument(words, usage)?;
    Ok(Control::Continue(format!(
        "{}: {}",
        renderer.paint(Role::Name, name),
        renderer.paint(Role::Metadata, debugger.runtime_address(name).await?)
    )))
}

async fn execute_where(debugger: &DebuggerHandle, renderer: Renderer) -> uscope::Result<Control> {
    let location = debugger.current_location().await?;
    let function = location
        .image
        .function
        .as_ref()
        .map_or("<unknown>", |function| function.name.as_ref());
    let source = location.image.source.as_ref().and_then(|source| {
        debugger
            .module_image()
            .source_file(source.file)
            .map(|file| format!("{}:{}", file.path.display(), source.line))
    });

    Ok(Control::Continue(match source {
        Some(source) => format!(
            "{} at {} ({})",
            renderer.paint(Role::Name, function),
            renderer.paint(Role::Metadata, source),
            renderer.paint(Role::Metadata, location.address)
        ),
        None => format!(
            "{} at {}",
            renderer.paint(Role::Name, function),
            renderer.paint(Role::Metadata, location.address)
        ),
    }))
}

async fn execute_print<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let argument = optional_argument(words, usage)?;
    match argument {
        Some(expression) => {
            let value = debugger
                .inspect(parse_value_expression(expression, usage)?)
                .await?;
            let output = value.type_info.as_ref().map_or_else(
                || format_untyped_state(expression, &value.state, renderer),
                |type_info| format_typed_state(type_info, expression, &value.state, renderer),
            );
            Ok(Control::Continue(output))
        }
        None => Ok(Control::Continue(format_variables(
            &debugger.variables().await?,
            renderer,
        ))),
    }
}

fn parse_value_expression(expression: &str, usage: &str) -> uscope::Result<ValueExpression> {
    let explicit_dereferences = expression.bytes().take_while(|byte| *byte == b'*').count();
    let path = &expression[explicit_dereferences..];
    // Components are opaque names, not code. Keeping their spelling broad
    // preserves source-path and linkage selectors without adding evaluation.
    let components = path.split('.').map(str::to_owned).collect::<Vec<_>>();
    if path.contains("->")
        || path.starts_with('&')
        || components.iter().any(String::is_empty)
        || path.chars().all(|character| character.is_ascii_digit())
    {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }

    Ok(ValueExpression {
        components: Arc::from(components),
        explicit_dereferences: u32::try_from(explicit_dereferences)
            .map_err(|_| Error::InvalidCommand(usage.to_owned()))?,
    })
}

fn optional_argument<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
) -> uscope::Result<Option<&'a str>> {
    let argument = words.next();
    if words.next().is_some() {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }
    Ok(argument)
}

async fn execute_globals<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let filter = optional_argument(words, usage)?.map(str::to_owned);
    let page = debugger
        .globals(uscope::GlobalVariableQuery {
            filter,
            ..uscope::GlobalVariableQuery::default()
        })
        .await?;
    let mut lines = page
        .variables
        .iter()
        .map(|entry| {
            let type_name = match &entry.variable.type_info {
                uscope::GlobalVariableType::Resolved(type_info) => type_info.name.as_ref(),
                uscope::GlobalVariableType::Unsupported(_) => "<unsupported type>",
                uscope::GlobalVariableType::Malformed(_) => "<malformed type>",
                _ => "<unknown type>",
            };
            format!(
                "{} ({})",
                renderer.paint(Role::Name, &entry.variable.qualified_name),
                renderer.paint(Role::Type, type_name)
            )
        })
        .collect::<Vec<_>>();
    let shown = u64::try_from(page.variables.len()).expect("page length fits u64");
    if page.offset.saturating_add(shown) < page.total {
        lines.push(
            renderer
                .paint(
                    Role::Warning,
                    format!(
                        "showing {}..{} of {}; use the API pagination fields for more",
                        page.offset,
                        page.offset.saturating_add(shown),
                        page.total
                    ),
                )
                .to_string(),
        );
    }
    Ok(Control::Continue(lines.join("\n")))
}

fn execute_help<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let command = words.next();
    if words.next().is_some() {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }
    Ok(Control::Continue(match command {
        Some(name) => {
            let command =
                command_named(name).ok_or_else(|| Error::InvalidCommand(format!("help {name}")))?;
            format_command_help(command, renderer)
        }
        None => format_help(renderer),
    }))
}

fn format_help(renderer: Renderer) -> String {
    let name_width = COMMANDS
        .iter()
        .map(|command| command.name.len())
        .max()
        .unwrap_or(0);
    let alias_width = COMMANDS
        .iter()
        .map(|command| command.aliases.join(", ").len())
        .max()
        .unwrap_or(0);
    let mut output = "commands:".to_owned();
    for command in COMMANDS {
        let aliases = command.aliases.join(", ");
        let rendered_aliases = if aliases.is_empty() {
            String::new()
        } else {
            renderer.paint(Role::Alias, &aliases).to_string()
        };
        write!(
            output,
            "\n  {}{}  {}{}  {}",
            renderer.paint(Role::Command, command.name),
            " ".repeat(name_width - command.name.len()),
            rendered_aliases,
            " ".repeat(alias_width - aliases.len()),
            command.summary
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("\n\nUse `help <command>` for aliases and usage.");
    output
}

fn format_command_help(command: &CommandSpec, renderer: Renderer) -> String {
    let mut output = format!("  {}", command.summary);
    if !command.aliases.is_empty() {
        write!(
            output,
            "\n  {}: {}",
            renderer.paint(Role::Muted, "aliases"),
            renderer.paint(Role::Alias, command.aliases.join(", "))
        )
        .expect("writing to a String cannot fail");
    }
    if command.usage != command.name {
        write!(
            output,
            "\n  {}: {}",
            renderer.paint(Role::Muted, "usage"),
            command.usage
        )
        .expect("writing to a String cannot fail");
    }
    output
}

async fn execute_break(
    debugger: &DebuggerHandle,
    argument: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let breakpoint = debugger
        .add_breakpoint(parse_breakpoint_spec(argument)?)
        .await?;
    Ok(Control::Continue(format_breakpoint(&breakpoint, renderer)))
}

async fn execute_list_breakpoints(
    debugger: &DebuggerHandle,
    renderer: Renderer,
) -> uscope::Result<Control> {
    Ok(Control::Continue(format_breakpoints(
        debugger.snapshot().await?.breakpoints.as_ref(),
        renderer,
    )))
}

async fn execute_delete_breakpoint(
    debugger: &DebuggerHandle,
    argument: &str,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    if argument == "all" {
        let removed = debugger.remove_all_breakpoints().await?;
        return Ok(Control::Continue(format!(
            "{} {} breakpoint{}",
            renderer.paint(Role::Success, "deleted"),
            removed.len(),
            if removed.len() == 1 { "" } else { "s" }
        )));
    }
    let id = argument
        .parse::<u64>()
        .map_err(|_| Error::InvalidCommand(usage.to_owned()))?;
    let removed = debugger.remove_breakpoint(BreakpointId::new(id)).await?;
    Ok(Control::Continue(format!(
        "{} breakpoint {}",
        renderer.paint(Role::Success, "deleted"),
        renderer.paint(Role::Metadata, removed.id)
    )))
}

fn parse_breakpoint_spec(argument: &str) -> uscope::Result<BreakpointSpec> {
    if let Ok(address) = parse_address(argument) {
        return Ok(BreakpointSpec::Address(VirtualAddress::new(address)));
    }
    if let Some((path, location)) = argument.rsplit_once(':') {
        if path.is_empty() || location.is_empty() {
            return Err(Error::InvalidCommand(
                "break <function|address|file:line|file:function>".to_owned(),
            ));
        }
        if let Ok(line) = location.parse::<u64>() {
            let line = LineNumber::new(line).ok_or_else(|| {
                Error::InvalidCommand("source line numbers are one-based".to_owned())
            })?;
            return Ok(BreakpointSpec::Source {
                path: PathBuf::from(path),
                line,
            });
        }
        return Ok(BreakpointSpec::FileFunction {
            path: PathBuf::from(path),
            function: location.to_owned(),
        });
    }
    Ok(BreakpointSpec::Function(argument.to_owned()))
}

async fn execute_step(
    debugger: &DebuggerHandle,
    kind: StepKind,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let reason = debugger.step(kind).await?;
    Ok(Control::Continue(
        format_stop_with_source(debugger, reason, renderer).await,
    ))
}

async fn format_backtrace(
    debugger: &DebuggerHandle,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let trace = debugger.backtrace().await?;
    let mut lines = Vec::with_capacity(trace.frames.len() + 1);

    for frame in trace.frames.iter() {
        let name = frame
            .function
            .as_ref()
            .map_or("<unknown>", |function| function.name.as_ref());
        let source = frame.source.as_ref().and_then(|source| {
            debugger
                .module_image()
                .source_file(source.file)
                .map(|file| format!("{}:{}", file.path.display(), source.line))
        });
        lines.push(format!(
            "{} {} in {}{}",
            renderer.paint(
                if frame.level == 0 {
                    Role::Current
                } else {
                    Role::Metadata
                },
                format_args!("#{:<2}", frame.level)
            ),
            renderer.paint(Role::Metadata, format_args!("{:#018x}", frame.instruction)),
            renderer.paint(Role::Name, name),
            source.map_or_else(String::new, |source| format!(
                " at {}",
                renderer.paint(Role::Metadata, source)
            ))
        ));
    }
    lines.push(format!(
        "{}: {:?}",
        renderer.paint(Role::Metadata, "unwind stopped"),
        trace.termination
    ));

    Ok(Control::Continue(lines.join("\n")))
}

async fn select_thread<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
    renderer: Renderer,
) -> uscope::Result<Control> {
    let value = one_argument(words, usage)?;
    let id = value
        .parse::<u64>()
        .map_err(|_| Error::InvalidCommand(format!("invalid thread ID: {value}")))?;
    debugger.select_thread(ThreadId::new(id)).await?;
    Ok(Control::Continue(format!(
        "{} thread {}",
        renderer.paint(Role::Success, "selected"),
        renderer.paint(Role::Metadata, id)
    )))
}

fn format_threads(snapshot: &StateSnapshot, renderer: Renderer) -> String {
    snapshot
        .threads
        .iter()
        .map(|thread| {
            let marker = if snapshot.selected_thread == Some(thread.id) {
                "*"
            } else {
                " "
            };
            let state = match &thread.state {
                ThreadState::Running => "running".to_owned(),
                ThreadState::Stopped {
                    reason: Some(reason),
                } => {
                    format!("stopped: {}", format_stop(reason.clone(), renderer))
                }
                ThreadState::Stopped { reason: None } => "stopped".to_owned(),
            };
            let marker = if marker == "*" {
                renderer.paint(Role::Current, marker).to_string()
            } else {
                marker.to_owned()
            };
            format!(
                "{marker} {} {state}",
                renderer.paint(Role::Metadata, thread.id)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_registers(
    registers: &RegisterSnapshot,
    byte_order: ByteOrder,
    renderer: Renderer,
) -> String {
    let name_width = registers
        .registers
        .iter()
        .map(|value| value.register.name.len())
        .max()
        .unwrap_or(0);

    registers
        .registers
        .iter()
        .map(|value| {
            format!(
                "{} {}",
                renderer.paint(
                    Role::Name,
                    format_args!("{:<name_width$}", value.register.name)
                ),
                renderer.paint(Role::Value, format_register_bytes(&value.bytes, byte_order))
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_variables(snapshot: &VariableSnapshot, renderer: Renderer) -> String {
    snapshot
        .variables
        .iter()
        .map(|variable| format_variable(variable, renderer))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_variable(variable: &Variable, renderer: Renderer) -> String {
    let Some(type_info) = variable.type_info.as_ref() else {
        return format_untyped_state(&variable.name, &variable.state, renderer);
    };
    format_typed_state(type_info, &variable.name, &variable.state, renderer)
}

fn format_untyped_state(name: &str, state: &VariableState, renderer: Renderer) -> String {
    let (role, value) = match state {
        VariableState::Unavailable(reason) => (Role::Warning, format!("<unavailable: {reason}>")),
        VariableState::Malformed(reason) => {
            (Role::Error, format!("<malformed: {}>", reason.description))
        }
        VariableState::Available { .. } => (Role::Warning, "<unknown value>".to_owned()),
    };
    format!(
        "({}) {} = {}",
        renderer.paint(Role::Type, "<unknown type>"),
        renderer.paint(Role::Name, name),
        renderer.paint(role, value)
    )
}

fn format_typed_state(
    type_info: &uscope::TypeInfo,
    name: &str,
    state: &VariableState,
    renderer: Renderer,
) -> String {
    let value = match state {
        VariableState::Available { value, .. } => renderer
            .paint(Role::Value, format_value_graph(value))
            .to_string(),
        VariableState::Unavailable(reason) => renderer
            .paint(Role::Warning, format!("<unavailable: {reason}>"))
            .to_string(),
        VariableState::Malformed(reason) => renderer
            .paint(Role::Error, format!("<malformed: {}>", reason.description))
            .to_string(),
    };
    format!(
        "({}) {} = {value}",
        renderer.paint(Role::Type, &type_info.name),
        renderer.paint(Role::Name, name)
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "the iterative renderer handles every node and aggregate state without recursive calls"
)]
fn format_value_graph(graph: &uscope::ValueGraph) -> String {
    const MAX_RENDER_DEPTH: usize = 64;
    const MAX_OUTPUT_BYTES: usize = 64 * 1024;
    enum Work {
        Node(uscope::ValueNodeId, usize),
        Text(String),
    }
    let mut output = String::new();
    let mut work = vec![Work::Node(graph.root_id(), 0)];
    while let Some(item) = work.pop() {
        if output.len() >= MAX_OUTPUT_BYTES {
            output.truncate(MAX_OUTPUT_BYTES);
            while !output.is_char_boundary(output.len()) {
                output.pop();
            }
            output.push_str("<truncated: OutputBytes>");
            break;
        }
        let Work::Node(id, depth) = item else {
            let Work::Text(text) = item else {
                unreachable!()
            };
            output.push_str(&text);
            continue;
        };
        if depth > MAX_RENDER_DEPTH {
            output.push_str("<truncated: AggregateDepth>");
            continue;
        }
        let Some(node) = graph.node(id) else {
            output.push_str("<malformed: invalid value node>");
            continue;
        };
        let value = match &node.state {
            uscope::ValueNodeState::Available(value) => value,
            uscope::ValueNodeState::Unavailable(reason) => {
                write!(output, "<unavailable: {reason}>").expect("String writes cannot fail");
                continue;
            }
            uscope::ValueNodeState::Malformed(reason) => {
                write!(output, "<malformed: {}>", reason.description)
                    .expect("String writes cannot fail");
                continue;
            }
            uscope::ValueNodeState::Truncated(limit) => {
                write!(output, "<truncated: {limit:?}>").expect("String writes cannot fail");
                continue;
            }
            uscope::ValueNodeState::Cycle { original } => {
                write!(output, "<cycle to #{}>", original.get())
                    .expect("String writes cannot fail");
                continue;
            }
            _ => {
                output.push_str("<unsupported value state>");
                continue;
            }
        };
        match value {
            uscope::VariableValue::Scalar(value) => {
                output.push_str(&format_scalar(&node.type_info, value));
            }
            uscope::VariableValue::Enumeration { value, matches } => {
                let raw = format_integer(*value);
                match matches.as_ref() {
                    [] => output.push_str(&raw),
                    [enumerator] => {
                        write!(output, "{} ({raw})", enumerator.name)
                            .expect("String writes cannot fail");
                    }
                    aliases => {
                        output.push_str(&raw);
                        output.push_str(" <");
                        for (index, alias) in aliases.iter().enumerate() {
                            if index != 0 {
                                output.push_str(", ");
                            }
                            output.push_str(&alias.name);
                        }
                        output.push('>');
                    }
                }
            }
            uscope::VariableValue::Address(value) => {
                let width = node
                    .type_info
                    .byte_size
                    .and_then(|size| usize::try_from(size.checked_mul(2)?).ok())
                    .unwrap_or(16);
                write!(output, "0x{:0width$x}", value.address.get())
                    .expect("String writes cannot fail");
            }
            uscope::VariableValue::ImplicitPointer => output.push_str("<implicit pointer>"),
            uscope::VariableValue::Array {
                elements, omitted, ..
            }
            | uscope::VariableValue::Slice {
                elements, omitted, ..
            } => {
                work.push(Work::Text("]".to_owned()));
                if *omitted != 0 {
                    work.push(Work::Text(format!("<{omitted} omitted>")));
                    if !elements.is_empty() {
                        work.push(Work::Text(", ".to_owned()));
                    }
                }
                for (index, element) in elements.iter().enumerate().rev() {
                    if index + 1 != elements.len() {
                        work.push(Work::Text(", ".to_owned()));
                    }
                    work.push(Work::Node(*element, depth + 1));
                }
                output.push('[');
            }
            uscope::VariableValue::Record {
                members,
                bases,
                omitted,
            } => {
                let mut children = Vec::with_capacity(bases.len() + members.len());
                for base in bases.iter() {
                    let name = graph
                        .node(base.value)
                        .map_or("<unknown base>", |node| node.type_info.name.as_ref());
                    children.push((format!("<base {name}> = "), base.value));
                }
                for member in members.iter().filter(|member| !member.member.artificial) {
                    children.push((
                        format!(
                            "{} = ",
                            member.member.name.as_deref().unwrap_or("<anonymous>")
                        ),
                        member.value,
                    ));
                }
                let child_count = children.len();
                work.push(Work::Text("}".to_owned()));
                if *omitted != 0 {
                    work.push(Work::Text(format!("<{omitted} omitted>")));
                    if child_count != 0 {
                        work.push(Work::Text(", ".to_owned()));
                    }
                }
                for (index, (label, child)) in children.into_iter().enumerate().rev() {
                    if index + 1 != child_count {
                        work.push(Work::Text(", ".to_owned()));
                    }
                    work.push(Work::Node(child, depth + 1));
                    work.push(Work::Text(label));
                }
                output.push('{');
            }
            uscope::VariableValue::Union { members, omitted } => {
                let children = members
                    .iter()
                    .filter(|member| !member.member.artificial)
                    .map(|member| {
                        (
                            format!(
                                "{} = ",
                                member.member.name.as_deref().unwrap_or("<anonymous>")
                            ),
                            member.value,
                        )
                    })
                    .collect::<Vec<_>>();
                let child_count = children.len();
                work.push(Work::Text("} <active member unknown>".to_owned()));
                if *omitted != 0 {
                    work.push(Work::Text(format!("<{omitted} omitted>")));
                    if child_count != 0 {
                        work.push(Work::Text(", ".to_owned()));
                    }
                }
                for (index, (label, child)) in children.into_iter().enumerate().rev() {
                    if index + 1 != child_count {
                        work.push(Work::Text(", ".to_owned()));
                    }
                    work.push(Work::Node(child, depth + 1));
                    work.push(Work::Text(label));
                }
                output.push('{');
            }
            uscope::VariableValue::Variant {
                discriminant,
                common_members,
                bases,
                active,
                omitted,
            } => {
                let mut children = Vec::new();
                for base in bases.iter() {
                    let name = graph
                        .node(base.value)
                        .map_or("<unknown base>", |node| node.type_info.name.as_ref());
                    children.push((format!("<base {name}> = "), base.value));
                }
                for member in common_members
                    .iter()
                    .filter(|member| !member.member.artificial)
                {
                    children.push((
                        format!(
                            "{} = ",
                            member.member.name.as_deref().unwrap_or("<anonymous>")
                        ),
                        member.value,
                    ));
                }
                if let Some(active) = active {
                    for member in active
                        .members
                        .iter()
                        .filter(|member| !member.member.artificial)
                    {
                        children.push((
                            format!(
                                "{} = ",
                                member.member.name.as_deref().unwrap_or("<anonymous>")
                            ),
                            member.value,
                        ));
                    }
                }
                let omitted = total_variant_omitted(*omitted, active.as_ref());
                let child_count = children.len();
                let suffix = match (active, discriminant) {
                    (Some(active), _) if child_count == 0 => active
                        .variant
                        .name
                        .as_deref()
                        .map_or_else(String::new, |name| format!("<{name}>")),
                    (None, Some(discriminant)) => {
                        format!("<no matching variant: {}>", format_integer(*discriminant))
                    }
                    (None, None) => "<no matching variant>".to_owned(),
                    _ => String::new(),
                };
                work.push(Work::Text(format!("}}{suffix}")));
                if omitted != 0 {
                    work.push(Work::Text(format!("<{omitted} omitted>")));
                    if child_count != 0 {
                        work.push(Work::Text(", ".to_owned()));
                    }
                }
                for (index, (label, child)) in children.into_iter().enumerate().rev() {
                    if index + 1 != child_count {
                        work.push(Work::Text(", ".to_owned()));
                    }
                    work.push(Work::Node(child, depth + 1));
                    work.push(Work::Text(label));
                }
                output.push('{');
            }
            _ => output.push_str("<unsupported value>"),
        }
    }
    output
}

fn format_scalar(type_info: &uscope::TypeInfo, value: &ScalarValue) -> String {
    let character = matches!(
        &type_info.kind,
        uscope::TypeKind::Base(base) if base.base_name.as_ref() == "char"
    );
    format_scalar_value(value, character)
}

fn format_integer(value: uscope::IntegerValue) -> String {
    match value {
        uscope::IntegerValue::Signed(value) => value.to_string(),
        uscope::IntegerValue::Unsigned(value) => value.to_string(),
        _ => "<unsupported integer value>".to_owned(),
    }
}

fn total_variant_omitted(common: u64, active: Option<&uscope::ActiveVariantValue>) -> u64 {
    common.saturating_add(active.map_or(0, |active| active.omitted))
}

fn format_scalar_value(value: &ScalarValue, character: bool) -> String {
    match value {
        ScalarValue::Boolean(value) => value.to_string(),
        ScalarValue::Signed(value) => {
            if character
                && let Ok(character) = u8::try_from(*value)
                && character.is_ascii_graphic()
            {
                return format!("{value} '{}'", char::from(character).escape_default());
            }
            value.to_string()
        }
        ScalarValue::Unsigned(value) => value.to_string(),
        ScalarValue::Floating(value) => format_float(*value),
        _ => "<unsupported scalar value>".to_owned(),
    }
}

fn format_float(value: FloatValue) -> String {
    match value {
        FloatValue::Binary32(bits) => f32::from_bits(bits).to_string(),
        FloatValue::Binary64(bits) => f64::from_bits(bits).to_string(),
        FloatValue::X87Extended {
            significand,
            sign_exponent,
        } => X87DoubleExtended::from_bits(
            u128::from(significand) | (u128::from(sign_exponent) << 64),
        )
        .to_string(),
        _ => "<unsupported floating-point format>".to_owned(),
    }
}

fn format_register_bytes(bytes: &[u8], byte_order: ByteOrder) -> String {
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");

    match byte_order {
        ByteOrder::Little => {
            for byte in bytes.iter().rev() {
                write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            }
        }
        ByteOrder::Big => {
            for byte in bytes {
                write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            }
        }
    }

    output
}

async fn format_stop_with_source(
    debugger: &DebuggerHandle,
    reason: StopReason,
    renderer: Renderer,
) -> String {
    let has_source_context = matches!(
        reason,
        StopReason::Breakpoint { .. } | StopReason::Step { .. }
    );
    let mut output = format_stop(reason, renderer);

    if has_source_context {
        match debugger.source_context(3).await {
            Ok(context) => {
                output.push('\n');
                output.push_str(&format_source_context(&context, renderer));
            }
            Err(error) => {
                write!(
                    output,
                    "\n{}: {error}",
                    renderer.paint(Role::Warning, "source unavailable")
                )
                .expect("writing to a String cannot fail");
            }
        }
    }

    output
}

fn format_source_context(context: &SourceContext, renderer: Renderer) -> String {
    let line_width = context
        .lines
        .last()
        .map_or(1, |line| line.number.to_string().len());
    let mut output = format!(
        "{}:{}",
        renderer.paint(Role::Metadata, context.file.path.display()),
        renderer.paint(Role::Current, context.location.line)
    );

    for line in context.lines.iter() {
        let current = line.number == context.location.line;
        let marker = if current {
            renderer.paint(Role::Current, "=>").to_string()
        } else {
            "  ".to_owned()
        };
        let number = if current {
            renderer
                .paint(Role::Current, format_args!("{:>line_width$}", line.number))
                .to_string()
        } else {
            renderer
                .paint(Role::Metadata, format_args!("{:>line_width$}", line.number))
                .to_string()
        };
        write!(output, "\n{marker} {number} | {}", line.text)
            .expect("writing to a String cannot fail");
    }

    output
}

fn one_argument<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
) -> uscope::Result<&'a str> {
    let argument = words
        .next()
        .ok_or_else(|| Error::InvalidCommand(usage.to_owned()))?;

    if words.next().is_some() {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }

    Ok(argument)
}

fn no_arguments<'a>(words: &mut impl Iterator<Item = &'a str>, usage: &str) -> uscope::Result<()> {
    if words.next().is_some() {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }
    Ok(())
}

fn parse_address(value: &str) -> uscope::Result<u64> {
    let value = value.strip_prefix("0x").unwrap_or(value);

    u64::from_str_radix(value, 16)
        .map_err(|_| Error::InvalidCommand(format!("invalid hexadecimal address: {value}")))
}

fn format_breakpoint(breakpoint: &Breakpoint, renderer: Renderer) -> String {
    if let [resolved] = breakpoint.locations.as_ref() {
        return match resolved.location {
            BreakpointLocation::Image(address) => {
                format!(
                    "{} set at image address {}",
                    renderer.paint(Role::Success, "breakpoint"),
                    renderer.paint(Role::Metadata, address)
                )
            }
            BreakpointLocation::Virtual(address) => {
                format!(
                    "{} set at virtual address {}",
                    renderer.paint(Role::Success, "breakpoint"),
                    renderer.paint(Role::Metadata, address)
                )
            }
        };
    }

    let mut output = format!(
        "{} {} set at {} locations",
        renderer.paint(Role::Success, "breakpoint"),
        renderer.paint(Role::Metadata, breakpoint.id),
        breakpoint.locations.len()
    );
    for resolved in breakpoint.locations.iter() {
        match resolved.location {
            BreakpointLocation::Image(address) => {
                write!(
                    output,
                    "\n  image address {}",
                    renderer.paint(Role::Metadata, address)
                )
            }
            BreakpointLocation::Virtual(address) => {
                write!(
                    output,
                    "\n  virtual address {}",
                    renderer.paint(Role::Metadata, address)
                )
            }
        }
        .expect("writing to a String cannot fail");
    }

    output
}

fn format_breakpoints(breakpoints: &[Breakpoint], renderer: Renderer) -> String {
    if breakpoints.is_empty() {
        return renderer.paint(Role::Metadata, "no breakpoints").to_string();
    }
    let mut output = String::new();
    for (index, breakpoint) in breakpoints.iter().enumerate() {
        if index != 0 {
            output.push('\n');
        }
        write!(
            output,
            "{}  {}  {} location{}",
            renderer.paint(Role::Metadata, breakpoint.id),
            renderer.paint(Role::Name, format_breakpoint_spec(&breakpoint.spec)),
            breakpoint.locations.len(),
            if breakpoint.locations.len() == 1 {
                ""
            } else {
                "s"
            }
        )
        .expect("writing to a String cannot fail");
        for resolved in breakpoint.locations.iter() {
            match resolved.location {
                BreakpointLocation::Image(address) => write!(
                    output,
                    "\n   image {}",
                    renderer.paint(Role::Metadata, address)
                ),
                BreakpointLocation::Virtual(address) => write!(
                    output,
                    "\n   virtual {}",
                    renderer.paint(Role::Metadata, address)
                ),
            }
            .expect("writing to a String cannot fail");
        }
    }
    output
}

fn format_breakpoint_spec(spec: &BreakpointSpec) -> String {
    match spec {
        BreakpointSpec::Function(function) => function.clone(),
        BreakpointSpec::Address(address) => address.to_string(),
        BreakpointSpec::Source { path, line } => format!("{}:{line}", path.display()),
        BreakpointSpec::FileFunction { path, function } => {
            format!("{}:{function}", path.display())
        }
    }
}

fn format_stop(reason: StopReason, renderer: Renderer) -> String {
    match reason {
        StopReason::Breakpoint { address } => {
            format!(
                "{} at breakpoint {}",
                renderer.paint(Role::Current, "stopped"),
                renderer.paint(Role::Metadata, address)
            )
        }
        StopReason::Step { kind } => match kind {
            StepKind::Instruction => format!(
                "{} after instruction step",
                renderer.paint(Role::Current, "stopped")
            ),
            StepKind::IntoSource => format!(
                "{} after source step",
                renderer.paint(Role::Current, "stopped")
            ),
            StepKind::OverSource => format!(
                "{} after source next",
                renderer.paint(Role::Current, "stopped")
            ),
            StepKind::Out => format!(
                "{} after frame return",
                renderer.paint(Role::Current, "stopped")
            ),
        },
        StopReason::Pause => format!("inferior {}", renderer.paint(Role::Current, "paused")),
        StopReason::Exception(exception) => format!(
            "{} by {} ({:#x})",
            renderer.paint(Role::Error, "stopped"),
            renderer.paint(Role::Error, exception.description),
            exception.code
        ),
        StopReason::Exec => format!(
            "inferior {} its executable image",
            renderer.paint(Role::Warning, "replaced")
        ),
        StopReason::ThreadExited { thread_id, status } => {
            format!(
                "thread {} {}: {}",
                renderer.paint(Role::Metadata, thread_id),
                renderer.paint(Role::Warning, "exited"),
                format_exit_status(status, renderer)
            )
        }
        StopReason::Unclassifiable { description } => {
            format!(
                "inferior {} for an unclassifiable reason: {description}",
                renderer.paint(Role::Error, "stopped")
            )
        }
        StopReason::Exited(ExitStatus::Code(code)) => {
            let role = if code == 0 {
                Role::Success
            } else {
                Role::Error
            };
            format!(
                "inferior {} with status {}",
                renderer.paint(role, "exited"),
                renderer.paint(role, code)
            )
        }
        StopReason::Exited(ExitStatus::Terminated(exception)) => format!(
            "inferior {} by {} ({:#x})",
            renderer.paint(Role::Error, "terminated"),
            renderer.paint(Role::Error, exception.description),
            exception.code
        ),
    }
}

fn format_exit_status(status: ExitStatus, renderer: Renderer) -> String {
    match status {
        ExitStatus::Code(code) => {
            let role = if code == 0 {
                Role::Success
            } else {
                Role::Error
            };
            format!("status {}", renderer.paint(role, code))
        }
        ExitStatus::Terminated(exception) => {
            format!(
                "{} ({:#x})",
                renderer.paint(Role::Error, exception.description),
                exception.code
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_registry_has_unique_names_and_aliases() {
        let mut names = std::collections::BTreeSet::new();
        for command in COMMANDS {
            assert!(
                names.insert(command.name),
                "duplicate command {}",
                command.name
            );
            for alias in command.aliases {
                assert!(names.insert(*alias), "duplicate command alias {alias}");
                assert_eq!(
                    command_named(alias).map(|found| found.name),
                    Some(command.name)
                );
            }
            assert_eq!(
                command_named(command.name).map(|found| found.name),
                Some(command.name)
            );
            assert!(
                command
                    .usage
                    .strip_prefix(command.name)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(' ')),
                "usage must begin with command name: {}",
                command.usage
            );
        }
    }

    #[test]
    fn generated_help_contains_every_registered_command() {
        let help = format_help(Renderer::new(false));
        for command in COMMANDS {
            assert!(
                help.contains(command.name),
                "missing command {}",
                command.name
            );
            let detail = format_command_help(command, Renderer::new(false));
            assert!(detail.contains(command.summary));
            assert_eq!(
                detail.contains("\n  usage:"),
                command.usage != command.name,
                "usage visibility disagrees for {}",
                command.name
            );
            if command.usage != command.name {
                assert!(detail.contains(command.usage));
            }
            for alias in command.aliases {
                assert!(help.contains(alias), "overview omitted alias {alias}");
                assert!(
                    detail.contains(alias),
                    "detailed help omitted alias {alias}"
                );
            }
        }
    }

    #[test]
    fn print_paths_preserve_components_and_apply_leading_dereferences_last() {
        let expression = parse_value_expression("**my_value.first.second.third", "usage")
            .expect("valid structural path");
        assert_eq!(
            expression.components.as_ref(),
            ["my_value", "first", "second", "third"]
        );
        assert_eq!(expression.explicit_dereferences, 2);

        let qualified = parse_value_expression("one.c::duplicate", "usage")
            .expect("qualified dotted global remains available to root resolution");
        assert_eq!(qualified.components.as_ref(), ["one", "c::duplicate"]);
        assert_eq!(qualified.explicit_dereferences, 0);

        let package = parse_value_expression("github.com/acme/my-pkg.global", "usage")
            .expect("language-qualified global remains available to root resolution");
        assert_eq!(
            package.components.as_ref(),
            ["github", "com/acme/my-pkg", "global"]
        );

        let template = parse_value_expression("Wrapper<int>::value", "usage")
            .expect("C++-qualified global remains available to root resolution");
        assert_eq!(template.components.as_ref(), ["Wrapper<int>::value"]);

        let source_qualified =
            parse_value_expression("/build/src/9-right.c::right::shared", "usage")
                .expect("source-qualified global remains available to root resolution");
        assert_eq!(
            source_qualified.components.as_ref(),
            ["/build/src/9-right", "c::right::shared"]
        );
    }

    #[test]
    fn print_paths_reject_reserved_and_malformed_structural_syntax() {
        for expression in [
            "",
            "*",
            ".pair",
            "pair.",
            "pair..first",
            "&pair.first",
            "pair->first",
            "42",
        ] {
            assert!(
                matches!(
                    parse_value_expression(expression, "print usage"),
                    Err(Error::InvalidCommand(message)) if message == "print usage"
                ),
                "accepted unsupported print expression {expression:?}"
            );
        }
    }

    #[test]
    fn history_uses_xdg_then_home_then_a_local_fallback() {
        assert_eq!(
            history_path_from(
                Some(std::ffi::OsStr::new("/state")),
                Some(std::ffi::OsStr::new("/home/jim"))
            ),
            PathBuf::from("/state/uscope/history")
        );
        assert_eq!(
            history_path_from(None, Some(std::ffi::OsStr::new("/home/jim"))),
            PathBuf::from("/home/jim/.local/state/uscope/history")
        );
        assert_eq!(
            history_path_from(None, None),
            PathBuf::from(".uscope_history")
        );
    }

    #[test]
    fn register_bytes_are_rendered_in_target_byte_order() {
        assert_eq!(
            format_register_bytes(&[0x78, 0x56, 0x34, 0x12], ByteOrder::Little),
            "0x12345678"
        );
        assert_eq!(
            format_register_bytes(&[0x12, 0x34, 0x56, 0x78], ByteOrder::Big),
            "0x12345678"
        );
    }

    #[test]
    fn char_rendering_escapes_quote_and_backslash() {
        let render = |value: i128| format_scalar_value(&ScalarValue::Signed(value), true);
        assert_eq!(render(65), "65 'A'");
        assert_eq!(render(39), r"39 '\''");
        assert_eq!(render(92), r"92 '\\'");
    }

    #[test]
    fn floating_values_preserve_special_signs_and_extended_precision() {
        assert_eq!(
            format_float(FloatValue::Binary32(f32::INFINITY.to_bits())),
            "inf"
        );
        assert_eq!(
            format_float(FloatValue::Binary64((-0.0_f64).to_bits())),
            "-0"
        );
        assert_eq!(
            format_float(FloatValue::X87Extended {
                significand: 0xc800_0000_0000_0000,
                sign_exponent: 0x4000,
            }),
            "3.125"
        );
    }

    #[test]
    fn variant_omission_count_includes_active_members() {
        let active = uscope::ActiveVariantValue {
            variant: uscope::Variant {
                name: Some("Ready".into()),
                selection: uscope::VariantSelection::Default,
                members: Arc::from([]),
            },
            members: Arc::from([]),
            omitted: 2,
        };
        assert_eq!(total_variant_omitted(0, Some(&active)), 2);
        assert_eq!(total_variant_omitted(u64::MAX, Some(&active)), u64::MAX);
    }
}
