use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::{env, thread};

use anyhow::{Context, Result};
use clap::Parser;
use rustc_apfloat::Float as _;
use rustc_apfloat::ieee::X87DoubleExtended;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::io::{AsyncBufReadExt, BufReader};
use uscope::{
    Breakpoint, BreakpointId, BreakpointLocation, BreakpointSpec, ByteOrder, Debugger,
    DebuggerHandle, Error, ExitStatus, FloatValue, LineNumber, RegisterSnapshot, ScalarValue,
    SourceContext, StateSnapshot, StepKind, StopReason, ThreadId, ThreadState, Variable,
    VariableSnapshot, VariableState, VirtualAddress,
};

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
    Cls,
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
        ["clear"],
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
        "print [variable]",
        "Print one or all visible variables"
    ),
    command!(Stepi, "stepi", ["si"], "stepi", "Step one instruction"),
    command!(Step, "step", ["s"], "step", "Step into at source level"),
    command!(Next, "next", ["n"], "next", "Step over at source level"),
    command!(
        Finish,
        "finish",
        [],
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
    command!(Cls, "cls", [], "cls", "Clear and redraw the terminal"),
    command!(Help, "help", ["?"], "help [command]", "Show command help"),
    command!(Quit, "quit", ["q"], "quit", "Exit uscope"),
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let debugger = Debugger::new(&args.executable).with_context(|| {
        format!(
            "failed to initialize debugger for {}",
            args.executable.display()
        )
    })?;

    let handle = debugger.handle();
    let result = run_with_interrupts(&handle, &args).await;
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down debugger");

    result?;
    shutdown?;

    Ok(())
}

async fn run_with_interrupts(debugger: &DebuggerHandle, args: &Args) -> Result<()> {
    let mut terminal = Box::pin(run(debugger, args));

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

async fn run(debugger: &DebuggerHandle, args: &Args) -> Result<()> {
    if !args.batch {
        println!("debugging {}", debugger.executable().display());
        io::stdout().flush()?;
    }

    for path in &args.command_files {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read command file {}", path.display()))?;

        if !run_lines(debugger, contents.lines(), &path.display().to_string()).await? {
            return Ok(());
        }
    }

    for (index, command) in args.commands.iter().enumerate() {
        if !run_line(debugger, command, &format!("--eval #{}", index + 1)).await? {
            return Ok(());
        }
    }

    if args.batch {
        if args.command_files.is_empty() && args.commands.is_empty() {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            let mut number = 0_u64;

            while let Some(line) = lines.next_line().await? {
                number = number.checked_add(1).expect("stdin line number overflow");

                if !run_line(debugger, &line, &format!("stdin:{number}")).await? {
                    break;
                }
            }
        }

        Ok(())
    } else {
        repl(debugger).await
    }
}

async fn run_lines<'a>(
    debugger: &DebuggerHandle,
    lines: impl Iterator<Item = &'a str>,
    source: &str,
) -> Result<bool> {
    for (index, line) in lines.enumerate() {
        if !run_line(debugger, line, &format!("{source}:{}", index + 1)).await? {
            return Ok(false);
        }
    }

    Ok(true)
}

async fn run_line(debugger: &DebuggerHandle, line: &str, source: &str) -> Result<bool> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(true);
    }

    match execute(debugger, line)
        .await
        .with_context(|| source.to_owned())?
    {
        Control::Continue(message) => {
            if !message.is_empty() {
                println!("{message}");
                io::stdout().flush()?;
            }

            Ok(true)
        }
        Control::ClearScreen => {
            print!("\x1b[2J\x1b[H");
            io::stdout().flush()?;
            Ok(true)
        }
        Control::Quit => Ok(false),
    }
}

async fn repl(debugger: &DebuggerHandle) -> Result<()> {
    let show_prompt = io::stdin().is_terminal() && io::stdout().is_terminal();
    if show_prompt {
        return interactive_repl(debugger).await;
    }
    stream_repl(debugger, false).await
}

async fn stream_repl(debugger: &DebuggerHandle, show_prompt: bool) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut number = 0_u64;

    loop {
        if show_prompt {
            print!("> ");
            io::stdout().flush()?;
        }

        let Some(line) = lines.next_line().await? else {
            if show_prompt {
                println!();
            }
            return Ok(());
        };
        number = number.checked_add(1).expect("REPL line number overflow");

        match run_line(debugger, &line, &format!("repl:{number}")).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => eprintln!("error: {error:#}"),
        }
    }
}

enum ReplInput {
    Line { number: u64, text: String },
    Eof,
    Failed(String),
}

enum ReplAck {
    Continue,
    Quit,
}

async fn interactive_repl(debugger: &DebuggerHandle) -> Result<()> {
    let (input_sender, mut input_receiver) = tokio::sync::mpsc::channel(1);
    let (ack_sender, ack_receiver) = mpsc::channel();
    let editor = thread::Builder::new()
        .name("uscope-line-editor".to_owned())
        .spawn(move || line_editor(&input_sender, &ack_receiver))?;

    let mut outcome = Ok(());
    let mut last_command = None;
    while let Some(input) = input_receiver.recv().await {
        let keep_running = match input {
            ReplInput::Line { number, text } => {
                let trimmed = text.trim();
                let command = if trimmed.is_empty() {
                    last_command.as_deref().unwrap_or(trimmed)
                } else {
                    if !trimmed.starts_with('#') {
                        last_command = Some(trimmed.to_owned());
                    }
                    trimmed
                };
                match run_line(debugger, command, &format!("repl:{number}")).await {
                    Ok(keep_running) => keep_running,
                    Err(error) => {
                        eprintln!("error: {error:#}");
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
) {
    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(error) => {
            let _ = input.blocking_send(ReplInput::Failed(error.to_string()));
            return;
        }
    };
    let history = history_path();
    if history.exists()
        && let Err(error) = editor.load_history(&history)
    {
        eprintln!(
            "warning: failed to load command history {}: {error}",
            history.display()
        );
    }
    let mut number = 0_u64;
    loop {
        match editor.readline("> ") {
            Ok(line) => {
                number = number.checked_add(1).expect("REPL line number overflow");
                if !line.trim().is_empty()
                    && let Err(error) = editor.add_history_entry(line.as_str())
                {
                    eprintln!("warning: failed to record command history: {error}");
                }
                if input
                    .blocking_send(ReplInput::Line { number, text: line })
                    .is_err()
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
    persist_history(&mut editor, &history);
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

fn persist_history(editor: &mut DefaultEditor, path: &std::path::Path) {
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!(
            "warning: failed to create history directory {}: {error}",
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
            "warning: failed to save command history {}: {error}",
            path.display()
        );
    }
}

enum Control {
    Continue(String),
    ClearScreen,
    Quit,
}

async fn execute(debugger: &DebuggerHandle, line: &str) -> uscope::Result<Control> {
    let mut words = line.split_whitespace();
    let entered = words.next().unwrap_or("");
    if entered.is_empty() {
        return Ok(Control::Continue(String::new()));
    }
    let spec = command_named(entered).ok_or_else(|| Error::InvalidCommand(entered.to_owned()))?;

    match spec.command {
        Command::Break => {
            let argument = one_argument(&mut words, "break <function|address>")?;
            execute_break(debugger, argument).await
        }
        Command::Breakpoints => {
            no_arguments(&mut words, "breakpoints")?;
            execute_list_breakpoints(debugger).await
        }
        Command::Info => {
            let argument = one_argument(&mut words, "info breakpoints")?;
            if argument != "breakpoints" && argument != "break" {
                return Err(Error::InvalidCommand(format!("info {argument}")));
            }
            execute_list_breakpoints(debugger).await
        }
        Command::Delete => {
            let usage = format!("{entered} <id|all>");
            let argument = one_argument(&mut words, &usage)?;
            execute_delete_breakpoint(debugger, argument, &usage).await
        }
        Command::Run => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.run().await?).await,
        )),
        Command::Continue => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.resume().await?).await,
        )),
        Command::Pause => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.pause().await?).await,
        )),
        Command::Print => execute_print(debugger, &mut words).await,
        Command::Stepi => execute_step(debugger, StepKind::Instruction).await,
        Command::Step => execute_step(debugger, StepKind::IntoSource).await,
        Command::Next => execute_step(debugger, StepKind::OverSource).await,
        Command::Finish => execute_step(debugger, StepKind::Out).await,
        Command::Examine => {
            let address = parse_address(one_argument(&mut words, "x <runtime-address>")?)?;

            Ok(Control::Continue(format!(
                "{address:#018x}: {:#018x}",
                debugger.read_word(VirtualAddress::new(address)).await?
            )))
        }
        Command::Address => {
            let name = one_argument(&mut words, "address <symbol>")?;

            Ok(Control::Continue(format!(
                "{name}: {}",
                debugger.runtime_address(name).await?
            )))
        }
        Command::Where => {
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
                Some(source) => format!("{function} at {source} ({})", location.address),
                None => format!("{function} at {}", location.address),
            }))
        }
        Command::List => Ok(Control::Continue(format_source_context(
            &debugger.source_context(3).await?,
        ))),
        Command::Backtrace => format_backtrace(debugger).await,
        Command::Registers => {
            let registers = debugger.registers().await?;

            Ok(Control::Continue(format_registers(
                &registers,
                registers.target.byte_order,
            )))
        }
        Command::Threads => {
            let snapshot = debugger.snapshot().await?;
            Ok(Control::Continue(format_threads(&snapshot)))
        }
        Command::Thread => select_thread(debugger, &mut words).await,
        Command::Cls => {
            no_arguments(&mut words, "cls")?;
            Ok(Control::ClearScreen)
        }
        Command::Help => execute_help(&mut words),
        Command::Quit => Ok(Control::Quit),
    }
}

fn command_named(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
}

async fn execute_print<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
) -> uscope::Result<Control> {
    let argument = optional_argument(words, "print [variable]")?;
    match argument {
        Some(name) => {
            if !is_identifier(name) {
                return Err(Error::InvalidCommand("print [variable]".to_owned()));
            }
            Ok(Control::Continue(format_variable(
                &debugger.variable(name).await?,
            )))
        }
        None => Ok(Control::Continue(format_variables(
            &debugger.variables().await?,
        ))),
    }
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

fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(characters.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && characters.all(|character| matches!(character, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
}

fn execute_help<'a>(words: &mut impl Iterator<Item = &'a str>) -> uscope::Result<Control> {
    let command = words.next();
    if words.next().is_some() {
        return Err(Error::InvalidCommand("help [command]".to_owned()));
    }
    Ok(Control::Continue(match command {
        Some(name) => {
            let command =
                command_named(name).ok_or_else(|| Error::InvalidCommand(format!("help {name}")))?;
            format_command_help(command)
        }
        None => format_help(),
    }))
}

fn format_help() -> String {
    let width = COMMANDS
        .iter()
        .map(|command| format_command_label(command).len())
        .max()
        .unwrap_or(0);
    let mut output = "commands:".to_owned();
    for command in COMMANDS {
        write!(
            output,
            "\n  {:width$}  {}",
            format_command_label(command),
            command.summary
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("\n\nUse `help <command>` for aliases and usage.");
    output
}

fn format_command_label(command: &CommandSpec) -> String {
    if command.aliases.is_empty() {
        command.usage.to_owned()
    } else {
        format!("{} ({})", command.usage, command.aliases.join(", "))
    }
}

fn format_command_help(command: &CommandSpec) -> String {
    let mut output = format!("{}\n  {}", command.usage, command.summary);
    if !command.aliases.is_empty() {
        write!(output, "\n  aliases: {}", command.aliases.join(", "))
            .expect("writing to a String cannot fail");
    }
    output
}

async fn execute_break(debugger: &DebuggerHandle, argument: &str) -> uscope::Result<Control> {
    let breakpoint = debugger
        .add_breakpoint(parse_breakpoint_spec(argument)?)
        .await?;
    Ok(Control::Continue(format_breakpoint(&breakpoint)))
}

async fn execute_list_breakpoints(debugger: &DebuggerHandle) -> uscope::Result<Control> {
    Ok(Control::Continue(format_breakpoints(
        debugger.snapshot().await?.breakpoints.as_ref(),
    )))
}

async fn execute_delete_breakpoint(
    debugger: &DebuggerHandle,
    argument: &str,
    usage: &str,
) -> uscope::Result<Control> {
    if argument == "all" {
        let removed = debugger.remove_all_breakpoints().await?;
        return Ok(Control::Continue(format!(
            "deleted {} breakpoint{}",
            removed.len(),
            if removed.len() == 1 { "" } else { "s" }
        )));
    }
    let id = argument
        .parse::<u64>()
        .map_err(|_| Error::InvalidCommand(usage.to_owned()))?;
    let removed = debugger.remove_breakpoint(BreakpointId::new(id)).await?;
    Ok(Control::Continue(format!(
        "deleted breakpoint {}",
        removed.id
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

async fn execute_step(debugger: &DebuggerHandle, kind: StepKind) -> uscope::Result<Control> {
    let reason = debugger.step(kind).await?;
    Ok(Control::Continue(
        format_stop_with_source(debugger, reason).await,
    ))
}

async fn format_backtrace(debugger: &DebuggerHandle) -> uscope::Result<Control> {
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
                .map(|file| format!(" at {}:{}", file.path.display(), source.line))
        });
        lines.push(format!(
            "#{:<2} {:#018x} in {name}{}",
            frame.level,
            frame.instruction,
            source.unwrap_or_default()
        ));
    }
    lines.push(format!("unwind stopped: {:?}", trace.termination));

    Ok(Control::Continue(lines.join("\n")))
}

async fn select_thread<'a>(
    debugger: &DebuggerHandle,
    words: &mut impl Iterator<Item = &'a str>,
) -> uscope::Result<Control> {
    let value = one_argument(words, "thread <id>")?;
    let id = value
        .parse::<u64>()
        .map_err(|_| Error::InvalidCommand(format!("invalid thread ID: {value}")))?;
    debugger.select_thread(ThreadId::new(id)).await?;
    Ok(Control::Continue(format!("selected thread {id}")))
}

fn format_threads(snapshot: &StateSnapshot) -> String {
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
                    format!("stopped: {}", format_stop(reason.clone()))
                }
                ThreadState::Stopped { reason: None } => "stopped".to_owned(),
            };
            format!("{marker} {} {state}", thread.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_registers(registers: &RegisterSnapshot, byte_order: ByteOrder) -> String {
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
                "{:<name_width$} {}",
                value.register.name,
                format_register_bytes(&value.bytes, byte_order)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_variables(snapshot: &VariableSnapshot) -> String {
    snapshot
        .variables
        .iter()
        .map(format_variable)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_variable(variable: &Variable) -> String {
    let type_name = variable
        .type_info
        .as_ref()
        .map_or("<unknown type>", |type_info| type_info.name.as_ref());
    let value = match &variable.state {
        VariableState::Available { value, .. } => format_scalar(variable, value),
        VariableState::Unavailable(reason) => format!("<unavailable: {reason}>"),
        VariableState::Malformed(reason) => format!("<malformed: {}>", reason.description),
    };
    format!("({type_name}) {} = {value}", variable.name)
}

fn format_scalar(variable: &Variable, value: &ScalarValue) -> String {
    match value {
        ScalarValue::Boolean(value) => value.to_string(),
        ScalarValue::Signed(value) => {
            if variable
                .type_info
                .as_ref()
                .is_some_and(|type_info| type_info.base_name.as_ref() == "char")
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

async fn format_stop_with_source(debugger: &DebuggerHandle, reason: StopReason) -> String {
    let has_source_context = matches!(
        reason,
        StopReason::Breakpoint { .. } | StopReason::Step { .. }
    );
    let mut output = format_stop(reason);

    if has_source_context {
        match debugger.source_context(3).await {
            Ok(context) => {
                output.push('\n');
                output.push_str(&format_source_context(&context));
            }
            Err(error) => {
                write!(output, "\nsource unavailable: {error}")
                    .expect("writing to a String cannot fail");
            }
        }
    }

    output
}

fn format_source_context(context: &SourceContext) -> String {
    let line_width = context
        .lines
        .last()
        .map_or(1, |line| line.number.to_string().len());
    let mut output = format!("{}:{}", context.file.path.display(), context.location.line);

    for line in context.lines.iter() {
        let marker = if line.number == context.location.line {
            "=>"
        } else {
            "  "
        };
        write!(
            output,
            "\n{marker} {:>line_width$} | {}",
            line.number, line.text
        )
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

fn format_breakpoint(breakpoint: &Breakpoint) -> String {
    if let [resolved] = breakpoint.locations.as_ref() {
        return match resolved.location {
            BreakpointLocation::Image(address) => {
                format!("breakpoint set at image address {address}")
            }
            BreakpointLocation::Virtual(address) => {
                format!("breakpoint set at virtual address {address}")
            }
        };
    }

    let mut output = format!(
        "breakpoint {} set at {} locations",
        breakpoint.id,
        breakpoint.locations.len()
    );
    for resolved in breakpoint.locations.iter() {
        match resolved.location {
            BreakpointLocation::Image(address) => {
                write!(output, "\n  image address {address}")
            }
            BreakpointLocation::Virtual(address) => {
                write!(output, "\n  virtual address {address}")
            }
        }
        .expect("writing to a String cannot fail");
    }

    output
}

fn format_breakpoints(breakpoints: &[Breakpoint]) -> String {
    if breakpoints.is_empty() {
        return "no breakpoints".to_owned();
    }
    let mut output = String::new();
    for (index, breakpoint) in breakpoints.iter().enumerate() {
        if index != 0 {
            output.push('\n');
        }
        write!(
            output,
            "{}  {}  {} location{}",
            breakpoint.id,
            format_breakpoint_spec(&breakpoint.spec),
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
                BreakpointLocation::Image(address) => write!(output, "\n   image {address}"),
                BreakpointLocation::Virtual(address) => write!(output, "\n   virtual {address}"),
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

fn format_stop(reason: StopReason) -> String {
    match reason {
        StopReason::Breakpoint { address } => {
            format!("stopped at breakpoint {address}")
        }
        StopReason::Step { kind } => match kind {
            StepKind::Instruction => "stopped after instruction step".to_owned(),
            StepKind::IntoSource => "stopped after source step".to_owned(),
            StepKind::OverSource => "stopped after source next".to_owned(),
            StepKind::Out => "stopped after frame return".to_owned(),
        },
        StopReason::Pause => "inferior paused".to_owned(),
        StopReason::Exception(exception) => format!(
            "stopped by {} ({:#x})",
            exception.description, exception.code
        ),
        StopReason::Exec => "inferior replaced its executable image".to_owned(),
        StopReason::ThreadExited { thread_id, status } => {
            format!(
                "thread {} exited: {}",
                thread_id,
                format_exit_status(status)
            )
        }
        StopReason::Unclassifiable { description } => {
            format!("inferior stopped for an unclassifiable reason: {description}")
        }
        StopReason::Exited(ExitStatus::Code(code)) => {
            format!("inferior exited with status {code}")
        }
        StopReason::Exited(ExitStatus::Terminated(exception)) => format!(
            "inferior terminated by {} ({:#x})",
            exception.description, exception.code
        ),
    }
}

fn format_exit_status(status: ExitStatus) -> String {
    match status {
        ExitStatus::Code(code) => format!("status {code}"),
        ExitStatus::Terminated(exception) => {
            format!("{} ({:#x})", exception.description, exception.code)
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
        }
    }

    #[test]
    fn generated_help_contains_every_registered_command() {
        let help = format_help();
        for command in COMMANDS {
            let label = format_command_label(command);
            assert!(help.contains(&label), "missing help label {label}");
            let detail = format_command_help(command);
            assert!(detail.contains(command.usage));
            assert!(detail.contains(command.summary));
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
    fn variable_identifier_grammar_reserves_expressions_for_later() {
        for valid in ["value", "_value", "value2"] {
            assert!(is_identifier(valid));
        }
        for invalid in ["", "2value", "value.member", "*value", "left + right"] {
            assert!(!is_identifier(invalid));
        }
    }

    #[test]
    fn char_rendering_escapes_quote_and_backslash() {
        let variable = |value: i128| uscope::Variable {
            kind: uscope::VariableKind::Local,
            name: "c".into(),
            declaration: None,
            type_info: Some(uscope::BaseType {
                name: "char".into(),
                base_name: "char".into(),
                encoding: uscope::BaseTypeEncoding::Signed,
                byte_size: 1,
            }),
            state: uscope::VariableState::Available {
                storage: uscope::VariableStorage::Memory(uscope::VirtualAddress::new(0x1000)),
                raw: std::sync::Arc::from([u8::try_from(value).expect("test char fits in u8")]),
                value: uscope::ScalarValue::Signed(value),
            },
        };
        let render = |value: i128| {
            let variable = variable(value);
            match &variable.state {
                uscope::VariableState::Available { value, .. } => format_scalar(&variable, value),
                _ => unreachable!(),
            }
        };
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
}
