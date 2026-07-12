use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use uscope::{
    Breakpoint, BreakpointId, BreakpointLocation, BreakpointSpec, ByteOrder, Debugger,
    DebuggerHandle, Error, ExitStatus, LineNumber, RegisterSnapshot, SourceContext, StateSnapshot,
    StepKind, StopReason, ThreadId, ThreadState, VirtualAddress,
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
        Control::Quit => Ok(false),
    }
}

async fn repl(debugger: &DebuggerHandle) -> Result<()> {
    let show_prompt = io::stdin().is_terminal() && io::stdout().is_terminal();
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

enum Control {
    Continue(String),
    Quit,
}

async fn execute(debugger: &DebuggerHandle, line: &str) -> uscope::Result<Control> {
    let mut words = line.split_whitespace();
    let command = words.next().unwrap_or("");

    match command {
        "break" | "b" => {
            let argument = one_argument(&mut words, "break <function|address>")?;
            execute_break(debugger, argument).await
        }
        "breakpoints" => {
            no_arguments(&mut words, "breakpoints")?;
            execute_list_breakpoints(debugger).await
        }
        "info" => {
            let argument = one_argument(&mut words, "info breakpoints")?;
            if argument != "breakpoints" && argument != "break" {
                return Err(Error::InvalidCommand(format!("info {argument}")));
            }
            execute_list_breakpoints(debugger).await
        }
        "delete" | "clear" => {
            let usage = format!("{command} <id|all>");
            let argument = one_argument(&mut words, &usage)?;
            execute_delete_breakpoint(debugger, argument, &usage).await
        }
        "run" | "r" => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.run().await?).await,
        )),
        "continue" | "c" => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.resume().await?).await,
        )),
        "pause" | "p" => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.pause().await?).await,
        )),
        "stepi" | "si" => execute_step(debugger, StepKind::Instruction).await,
        "step" | "s" => execute_step(debugger, StepKind::IntoSource).await,
        "next" | "n" => execute_step(debugger, StepKind::OverSource).await,
        "finish" => execute_step(debugger, StepKind::Out).await,
        "x" => {
            let address = parse_address(one_argument(&mut words, "x <runtime-address>")?)?;

            Ok(Control::Continue(format!(
                "{address:#018x}: {:#018x}",
                debugger.read_word(VirtualAddress::new(address)).await?
            )))
        }
        "address" => {
            let name = one_argument(&mut words, "address <symbol>")?;

            Ok(Control::Continue(format!(
                "{name}: {}",
                debugger.runtime_address(name).await?
            )))
        }
        "where" => {
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
        "list" | "l" => Ok(Control::Continue(format_source_context(
            &debugger.source_context(3).await?,
        ))),
        "backtrace" | "bt" => format_backtrace(debugger).await,
        "registers" | "regs" => {
            let registers = debugger.registers().await?;

            Ok(Control::Continue(format_registers(
                &registers,
                registers.target.byte_order,
            )))
        }
        "threads" => {
            let snapshot = debugger.snapshot().await?;
            Ok(Control::Continue(format_threads(&snapshot)))
        }
        "thread" => select_thread(debugger, &mut words).await,
        "quit" | "q" => Ok(Control::Quit),
        "" => Ok(Control::Continue(String::new())),
        other => Err(Error::InvalidCommand(other.to_owned())),
    }
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
}
